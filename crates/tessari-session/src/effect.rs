//! Whether a script changes anything, which decides **where** it may run.
//!
//! Distinct from [`crate::identity`]'s `Needs`, which decides **who** may run
//! it, and the two do not reduce to one another: `Needs::Administer` covers
//! `GRANT` and `BACKUP` alike, one of which changes the store and one of which
//! only reads all of it. A node deciding whether it may take a statement needs
//! the second question answered, and asking the first would send every backup
//! to the leader for no reason.
//!
//! Kept beside `identity` rather than in the language crate so that the two
//! exhaustive matches sit together: adding a statement fails to compile until
//! both questions are answered, and neither can quietly drift from the other.

use tessari_encoding::Roles;
use tessari_ql::{Script, StatementKind};

use crate::error::{Error, Result};

/// Where running something has to happen.
///
/// **Not "does it change the store"**, which is the neighbouring question and
/// not the one routing asks. The two part company on the local half (ADR-0020
/// §3): `DEFINE NODE` changes this store and must still run *here*, because
/// what it changes is this machine's own identity, and sending it to the leader
/// would reconfigure the leader instead. A classifier that answered *is this a
/// change* would forward it, and the operator draining one node would quietly
/// drain another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    /// Runs wherever it was asked.
    ///
    /// Either it changes nothing, or what it changes is local to this node and
    /// never travels — both answer "here", which is the only thing routing
    /// needs from them.
    Read,
    /// Must run where writes are taken, which is the leader of what it touches.
    Write,
}

impl Effect {
    /// What running this statement would do.
    ///
    /// # Every statement is named, and there is no catch-all
    ///
    /// `identity::Needs::of` records why, from the day a `_ => Write` arm let
    /// `READ` fall through it: a catch-all mis-classifies a **new** statement
    /// silently. Here the cost of that is higher than a wrong permission.
    /// Kill criterion 4 of this goal states it — permissive-wrong costs a
    /// needless hop, strict-wrong **commits a write on a follower**, which is
    /// the split brain the whole design is arranged to prevent.
    ///
    /// So "default to `Write` when it cannot be decided" is about judgement on a
    /// statement somebody has actually looked at. It is not a licence for a
    /// wildcard arm, which would turn *"nobody has looked at this yet"* into a
    /// routing decision that compiles.
    pub const fn of(kind: &StatementKind) -> Self {
        match kind {
            // Reads, including the ones that look heavier than they are.
            // `BACKUP` streams the whole log and writes nothing to the store —
            // it is the largest read in the language, not a write.
            // A binding and an answer hold an **expression**, and no expression
            // in this language writes. What one may hold is a read, which is
            // already a read. Both stay `Read` for the same reason `SELECT`
            // does, and `of_script` still sees the write if one stands beside
            // them in the same block.
            StatementKind::Let { .. }
            | StatementKind::Return { .. }
            // A refusal holds an expression and changes nothing. It is a read
            // for the same reason a binding is, and `of_script` still sees the
            // write standing beside it in the same block.
            | StatementKind::Throw { .. }
            | StatementKind::Select(_)
            | StatementKind::Explain(_)
            | StatementKind::Get { .. }
            | StatementKind::Keys { .. }
            | StatementKind::Read { .. }
            | StatementKind::Info { .. }
            | StatementKind::Backup { .. } => Self::Read,

            // `USE` and the transaction verbs change what the *next* statement
            // runs in, and touch nothing themselves. They are reads here for the
            // same reason `Needs` makes them reads: refusing them would leave a
            // read-only node unable to say which database it is reading, or to
            // group its reads.
            //
            // This is safe only because the answer below is taken over the whole
            // script: `BEGIN` is not a write, but a block containing an `UPDATE`
            // is, and `of_script` is what sees that.
            StatementKind::Use { .. }
            | StatementKind::Begin
            | StatementKind::Commit
            | StatementKind::Cancel => Self::Read,

            // Structure.
            StatementKind::DefineNamespace { .. }
            | StatementKind::DefineDatabase { .. }
            | StatementKind::DefineTable { .. }
            | StatementKind::DefineSpace { .. }
            | StatementKind::DefineBucket { .. }
            | StatementKind::DefineIndex { .. }
            | StatementKind::DefineField { .. }
            | StatementKind::DefineAnalyzer { .. }
            | StatementKind::DropTable { .. }
            | StatementKind::DropIndex { .. }
            | StatementKind::DropField { .. }
            | StatementKind::DropAnalyzer { .. }
            | StatementKind::DropDatabase { .. }
            | StatementKind::DropNamespace { .. }
            | StatementKind::AlterTable { .. }
            | StatementKind::AlterField { .. }
            | StatementKind::RebuildIndex { .. } => Self::Write,

            // Who may reach it. Administering in `Needs`, and a write here:
            // a grant is a record like any other and must be decided in one
            // place, which is the leader.
            StatementKind::DefineUser { .. }
            | StatementKind::AlterUser { .. }
            | StatementKind::DropUser { .. }
            | StatementKind::Grant { .. }
            | StatementKind::Revoke { .. }
            | StatementKind::GrantAuthority { .. }
            | StatementKind::RevokeAuthority { .. } => Self::Write,

            // Topology, and the two halves part company here (ADR-0020 §3).
            //
            // `DEFINE NODE` writes to `META` rather than the log (Q-100) — it
            // is a change to this store, and still a `Read` *for routing*,
            // because what it changes is the local half. Forwarded, it would
            // reconfigure the leader's identity instead of this node's, so an
            // operator draining a follower would drain the leader. It is also
            // the only way back: a node that has just dropped `WRITABLE` must
            // still be able to take the statement that returns it, and a
            // classification of `Write` would make read-only a one-way door
            // with no spelling for reopening it.
            //
            // It is safe on a node that may not write for the reason it is not
            // routable: `META` is not replicated, so this cannot diverge two
            // stores.
            StatementKind::DefineNode { .. } => Self::Read,
            // `DEFINE REPLICA` is the opposite half and stays a write: it is a
            // catalog record, commits in the transaction that issued it, and
            // reaches every node through the ordinary apply path (ADR-0009).
            // `DROP REPLICA` is the inverse of that half and travels the same
            // way: it removes a catalog record, so it is a write and it reaches
            // every node. `DROP NODE` has no arm here because it has no
            // statement — the parser refuses it and says why.
            StatementKind::DefineReplica { .. } | StatementKind::DropReplica { .. } => Self::Write,
            // A consumer's **declaration** is a catalog record and replicates,
            // exactly as a replica's does; whether it is running on this machine
            // is local and is not part of the record. So both forms are writes,
            // and a follower that received one starts its own consumer in the
            // same group — which is the behaviour wanted, because the broker then
            // spreads the partitions across them.
            StatementKind::DefineConsumer { .. } | StatementKind::DropConsumer { .. } => {
                Self::Write
            }

            // Records and files. `UPDATE` and `DELETE … WHERE` read to find
            // their targets and then change them, which is exactly the shape a
            // keyword test gets wrong.
            StatementKind::Create { .. }
            | StatementKind::Update { .. }
            | StatementKind::Upsert { .. }
            | StatementKind::Delete { .. }
            | StatementKind::DeleteWhere { .. }
            | StatementKind::Relate { .. }
            | StatementKind::Set { .. }
            | StatementKind::Del { .. }
            | StatementKind::Put { .. } => Self::Write,
        }
    }

    /// What running this whole script would do.
    ///
    /// **Any write makes the script a write.** A script is one unit of work and
    /// routes as one: `BEGIN; SELECT …; UPDATE …; COMMIT;` opens with two
    /// statements that read, and sending it where its `UPDATE` cannot commit is
    /// the failure this criterion is named for. Deciding per statement, or by
    /// the first one, produces exactly that.
    #[must_use]
    pub fn of_script(script: &Script) -> Self {
        if script
            .statements
            .iter()
            .any(|statement| matches!(Self::of(&statement.kind), Self::Write))
        {
            Self::Write
        } else {
            Self::Read
        }
    }
}

/// Refuse a script this node may not take.
///
/// The refusal is the half of the criterion this wave builds. Forwarding it to
/// the leader is the other half and lives with routing, not here — a classifier
/// that also moved statements would be the second router ADR-0019 warns against.
///
/// # Errors
///
/// Returns [`Error::NotWritable`] when the script writes and this node does not
/// hold [`Roles::WRITABLE`].
pub fn admits(roles: Roles, script: &Script) -> Result<Effect> {
    let effect = Effect::of_script(script);
    if matches!(effect, Effect::Write) && !roles.has(Roles::WRITABLE) {
        return Err(Error::NotWritable { span: script.span });
    }
    Ok(effect)
}
