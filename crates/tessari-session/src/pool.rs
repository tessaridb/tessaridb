//! The pool classifier: which family a script belongs to, for the admission
//! gate a declared pool bounds (SG3 W3).
//!
//! One family per script — the strongest present — so "first matching
//! declared pool wins" can never double-count one script in two pools. The
//! families are the spec section's three: a script that defines, alters or
//! drops a catalog object is a `Definitions` script no matter what else it
//! holds; else a script that writes is a `Writes` script; else it reads.

use tessari_ql::{Script, StatementKind};
use tessari_storage::{Catalog, PoolSlot};

use crate::effect::Effect;
use crate::error::{Error, Result};
use crate::session::Session;

/// The family a script belongs to. Ordered strongest last so the script's
/// family is the maximum of its statements' families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PoolFamily {
    /// Reads and changes nothing.
    Reads,
    /// Writes records without declaring anything.
    Writes,
    /// Defines, alters or drops a catalog object.
    Definitions,
}

/// The script's family: `Definitions` when any statement defines, alters or
/// drops a catalog object, else `Writes` when any statement writes, else
/// `Reads`.
pub(crate) fn family_of_script(script: &Script) -> PoolFamily {
    let mut family = PoolFamily::Reads;
    for statement in &script.statements {
        family = family.max(classify(&statement.kind));
    }
    family
}

/// One statement's family. A definition outranks a write outranks a read, so
/// a mixed script lands in the strongest family present.
fn classify(kind: &StatementKind) -> PoolFamily {
    if is_definition(kind) {
        PoolFamily::Definitions
    } else if Effect::of(kind) == Effect::Write {
        PoolFamily::Writes
    } else {
        PoolFamily::Reads
    }
}

/// Whether the statement defines, alters or drops a catalog object.
///
/// # Every variant is named, and there is no catch-all
///
/// For [`Effect::of`]'s reason, one level over: a catch-all would
/// mis-classify a **new** statement silently, and here the cost is a pool
/// that does not bound what its operator believed it bounded. Borderline
/// calls, stated: `TRUNCATE` and `REBUILD INDEX` rewrite data and are
/// writes; `ANALYZE TABLE` and `CHECK TABLE` read, routed as reads for
/// [`Effect::of`]'s reason; a vault's seal and unseal move only this
/// process's hold on the master key and ride [`Effect::of`]'s routing as
/// reads, its passphrase change rewrites the row and is a write — what a
/// vault *is* is set at `DEFINE VAULT` and `DROP VAULT`; `GRANT`, `REVOKE`
/// and the token and certificate statements change what the catalog
/// declares about authority, and are definitions.
fn is_definition(kind: &StatementKind) -> bool {
    match kind {
        StatementKind::DefineNamespace { .. }
        | StatementKind::DefineDatabase { .. }
        | StatementKind::DefineTable { .. }
        | StatementKind::DefineCollection { .. }
        | StatementKind::DefineGraph { .. }
        | StatementKind::DefineEdge { .. }
        | StatementKind::DefineSpace { .. }
        | StatementKind::DefineTopic { .. }
        | StatementKind::DefineGroup { .. }
        | StatementKind::DefineBucket { .. }
        | StatementKind::DefineVector { .. }
        | StatementKind::DefineGeo { .. }
        | StatementKind::DefineVault { .. }
        | StatementKind::DefineIndex { .. }
        | StatementKind::DefineField { .. }
        | StatementKind::DefineAnalyzer { .. }
        | StatementKind::DefineSearch { .. }
        | StatementKind::DefineSynonyms { .. }
        | StatementKind::DefineStopwords { .. }
        | StatementKind::DefineUser { .. }
        | StatementKind::DefineNode { .. }
        | StatementKind::DefineFailover { .. }
        | StatementKind::DefineTopology { .. }
        | StatementKind::DefineBalancer { .. }
        | StatementKind::DefinePool { .. }
        | StatementKind::DefineReplica { .. }
        | StatementKind::DefineReplication { .. }
        | StatementKind::DefineConsumer { .. }
        | StatementKind::DefineTopicConsumer { .. }
        | StatementKind::DefineQueue { .. }
        | StatementKind::DefineSeries { .. }
        | StatementKind::DefineRollup { .. }
        | StatementKind::DefineView { .. }
        | StatementKind::DefineEvent { .. }
        | StatementKind::DefineFeed { .. }
        | StatementKind::DefineParam { .. }
        | StatementKind::AlterUser { .. }
        | StatementKind::AlterNamespace { .. }
        | StatementKind::AlterGroup { .. }
        | StatementKind::AlterTopology { .. }
        | StatementKind::AlterField { .. }
        | StatementKind::AlterReplica { .. }
        | StatementKind::AlterReplication { .. }
        | StatementKind::AlterTable { .. }
        | StatementKind::DropUser { .. }
        | StatementKind::DropField { .. }
        | StatementKind::DropTable { .. }
        | StatementKind::DropIndex { .. }
        | StatementKind::DropSearch { .. }
        | StatementKind::DropSynonyms { .. }
        | StatementKind::DropStopwords { .. }
        | StatementKind::DropAnalyzer { .. }
        | StatementKind::DropReplica { .. }
        | StatementKind::DropReplication { .. }
        | StatementKind::DropDatabase { .. }
        | StatementKind::DropNamespace { .. }
        | StatementKind::DropGraph { .. }
        | StatementKind::DropEdge { .. }
        | StatementKind::DropVector { .. }
        | StatementKind::DropGeo { .. }
        | StatementKind::DropVault { .. }
        | StatementKind::DropRollup { .. }
        | StatementKind::DropSeries { .. }
        | StatementKind::DropQueue { .. }
        | StatementKind::DropView { .. }
        | StatementKind::DropEvent { .. }
        | StatementKind::DropFeed { .. }
        | StatementKind::DropParam { .. }
        | StatementKind::DropGroup { .. }
        | StatementKind::DropConsumer { .. }
        | StatementKind::DropTopicConsumer { .. }
        | StatementKind::DropTopology { .. }
        | StatementKind::DropBalancer
        | StatementKind::DropPool { .. }
        | StatementKind::Grant { .. }
        | StatementKind::GrantAuthority { .. }
        | StatementKind::Revoke { .. }
        | StatementKind::RevokeAuthority { .. }
        | StatementKind::RevokeCertificate { .. }
        | StatementKind::CreateJoinToken { .. }
        | StatementKind::CreateLinkToken { .. }
        | StatementKind::FinalizeFormat => true,
        // What the drop it wraps is, for [`Effect::of`]'s reason.
        StatementKind::DropIfExists(dropped) => is_definition(dropped),
        StatementKind::Use { .. }
        | StatementKind::ReadTopic { .. }
        | StatementKind::HandTopic(..)
        | StatementKind::AckTopic { .. }
        | StatementKind::NackTopic { .. }
        | StatementKind::TrimTopic { .. }
        | StatementKind::Explain(_)
        | StatementKind::Info { .. }
        | StatementKind::Backup { .. }
        | StatementKind::Restore { .. }
        | StatementKind::Relate { .. }
        | StatementKind::Claim { .. }
        | StatementKind::ClaimRecord { .. }
        | StatementKind::Release { .. }
        | StatementKind::ReleaseAll { .. }
        | StatementKind::CheckTable { .. }
        | StatementKind::AnalyzeTable { .. }
        | StatementKind::RebuildIndex { .. }
        | StatementKind::Create { .. }
        | StatementKind::Insert { .. }
        | StatementKind::Select(_)
        | StatementKind::Reveal { .. }
        | StatementKind::AddRecipient { .. }
        | StatementKind::RemoveRecipient { .. }
        | StatementKind::UnsealVault { .. }
        | StatementKind::ChangeVaultPassphrase { .. }
        | StatementKind::SealVault { .. }
        | StatementKind::Update { .. }
        | StatementKind::Throw { .. }
        | StatementKind::Upsert { .. }
        | StatementKind::Delete { .. }
        | StatementKind::DeleteEdge { .. }
        | StatementKind::DeleteWhere { .. }
        | StatementKind::Truncate { .. }
        | StatementKind::UpdateWhere { .. }
        | StatementKind::DeleteSpan { .. }
        | StatementKind::Get { .. }
        | StatementKind::Set { .. }
        | StatementKind::Incr { .. }
        | StatementKind::Structure { .. }
        | StatementKind::Expire { .. }
        | StatementKind::Persist { .. }
        | StatementKind::Del { .. }
        | StatementKind::Put { .. }
        | StatementKind::Read { .. }
        | StatementKind::Keys { .. }
        | StatementKind::Let { .. }
        | StatementKind::Return { .. }
        | StatementKind::Begin
        | StatementKind::Commit
        | StatementKind::Cancel
        | StatementKind::Savepoint { .. }
        | StatementKind::RollbackTo { .. }
        | StatementKind::ReleaseSavepoint { .. }
        | StatementKind::Verify => false,
    }
}

/// The pool gate: resolve the namespace's declared pools against the
/// script's classifier and take the first matching pool's slot for the
/// whole run. `None` when nothing bounds the script — no namespace
/// selected, no pools declared, no clause matched — which is the default
/// pool admitting anything.
///
/// # One catalog pass per script, deliberately not cached
///
/// The catalog is the authority: a TTL cache would let a fresh pool go
/// unenforced for its first interval, and a pool that reached one request
/// and not the next is an admission decision two callers would describe
/// differently. The pass is read-only and bounded; its cost is stated here
/// and measured by the noisy-tenant exercise this subgoal ships (W4).
///
/// # `carried`
///
/// A peer-carried script names its tenancy in a built `USE` rather than in
/// the session's selection, so the caller passes what it knows; absent
/// that, the session's own selection is the classifier's question.
impl Session<'_> {
    pub(crate) async fn admit_pool(
        &self,
        script: &Script,
        carried: Option<(&str, Option<&str>)>,
    ) -> Result<Option<PoolSlot>> {
        let namespace = carried.map_or_else(|| self.namespace(), |(namespace, _)| Some(namespace));
        let Some(namespace) = namespace else {
            // Nothing is selected, so nothing classifies — a script that
            // needed a namespace refuses on its own terms when it runs.
            return Ok(None);
        };
        let database = carried
            .and_then(|(_, database)| database)
            .or_else(|| self.database());

        let family = family_of_script(script);
        let user = self.signed_in().map(|user| user.name.clone());

        // The one read-only pass: namespace id, the namespace's pools, and
        // the session database's id when a pool names one.
        let mut transaction = self.store.begin().await?;
        let Ok(found) = Catalog::new(&mut transaction).namespace_id(namespace).await else {
            // The gate never refuses what the store would admit: a catalog
            // read that errors here errors for the script itself when it
            // runs, where the refusal names its real cause.
            transaction.rollback();
            return Ok(None);
        };
        let Some(namespace_id) = found else {
            transaction.rollback();
            return Ok(None);
        };
        let catalog = Catalog::new(&mut transaction);
        let Ok(mut pools) = catalog.pools(namespace_id).await else {
            transaction.rollback();
            return Ok(None);
        };
        let database_id = if pools.iter().any(|held| held.pool.database().is_some()) {
            match database {
                None => None,
                Some(name) => catalog.database_id(namespace_id, name).await.ok().flatten(),
            }
        } else {
            None
        };
        transaction.rollback();
        if pools.is_empty() {
            return Ok(None);
        }

        // First matching declared pool wins, in name order — the sort that
        // makes the winner deterministic; one family per script is what
        // makes "first wins" well-defined (the module's header).
        pools.sort_by(|a, b| a.name.cmp(&b.name));
        for definition in &pools {
            let pool = &definition.pool;
            if pool.database().is_some() && pool.database() != database_id {
                continue;
            }
            if pool.user().is_some() && pool.user() != user.as_deref() {
                continue;
            }
            let bounded = match family {
                PoolFamily::Reads => pool.kinds().reads,
                PoolFamily::Writes => pool.kinds().writes,
                PoolFamily::Definitions => pool.kinds().definitions,
            };
            if !bounded {
                continue;
            }
            return match self
                .store
                .pool_take(namespace, &definition.name, pool.max_concurrent())
            {
                Some(slot) => Ok(Some(slot)),
                None => Err(Error::PoolAtCapacity {
                    pool: format!("{namespace}:{}", definition.name),
                    span: script
                        .statements
                        .first()
                        .map_or_else(|| tessari_ql::Span::new(0, 0), |held| held.span),
                }),
            };
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{PoolFamily, family_of_script};
    use tessari_ql::parse;

    fn family_of(source: &str) -> PoolFamily {
        family_of_script(&parse(source).expect("a parseable script"))
    }

    #[test]
    fn a_pure_read_is_a_reads_script() {
        assert_eq!(family_of("SELECT * FROM orders;"), PoolFamily::Reads);
        assert_eq!(
            family_of("INFO FOR TABLE orders;"),
            PoolFamily::Reads,
            "an INFO changes nothing"
        );
        assert_eq!(
            family_of("BEGIN; SELECT * FROM orders; CANCEL;"),
            PoolFamily::Reads,
            "transaction verbs ride the statements beside them"
        );
        assert_eq!(
            family_of("ANALYZE TABLE orders;"),
            PoolFamily::Reads,
            "statistics are this node's own, routed as a read"
        );
    }

    #[test]
    fn a_write_present_makes_the_script_a_writes_one() {
        assert_eq!(
            family_of("CREATE orders:1 = { placed: true };"),
            PoolFamily::Writes
        );
        assert_eq!(
            family_of("SELECT * FROM orders; UPDATE orders:1 SET placed = false;"),
            PoolFamily::Writes,
            "one write beside reads lifts the whole script"
        );
        assert_eq!(
            family_of("TRUNCATE orders;"),
            PoolFamily::Writes,
            "truncate rewrites the table's data and declares nothing"
        );
        assert_eq!(
            family_of("REBUILD INDEX orders_by_placed ON orders;"),
            PoolFamily::Writes,
            "a rebuild rewrites index data, not the catalog"
        );
    }

    #[test]
    fn a_definition_present_makes_the_script_a_definitions_one() {
        assert_eq!(
            family_of("DEFINE TABLE orders SCHEMALESS;"),
            PoolFamily::Definitions
        );
        assert_eq!(
            family_of("DROP TABLE orders;"),
            PoolFamily::Definitions,
            "a drop is a definition the other way"
        );
        assert_eq!(
            family_of("ALTER TABLE orders ADD FIELD note TYPE string;"),
            PoolFamily::Definitions
        );
        assert_eq!(
            family_of("DEFINE USER ada ON prod.work ROLE viewer PASSWORD 'secret';"),
            PoolFamily::Definitions
        );
        assert_eq!(
            family_of("GRANT read ON orders TO ada;"),
            PoolFamily::Definitions,
            "an authority change declares what the catalog holds"
        );
        assert_eq!(
            family_of("DEFINE POOL web WITH MAX CONCURRENT 2 FOR READS;"),
            PoolFamily::Definitions
        );
        assert_eq!(
            family_of("CREATE orders:1 = { placed: true }; DEFINE TABLE archived SCHEMALESS;"),
            PoolFamily::Definitions,
            "the strongest family present wins over the write beside it"
        );
        assert_eq!(
            family_of("DROP TABLE IF EXISTS orders;"),
            PoolFamily::Definitions,
            "a guarded drop is the drop it guards"
        );
    }

    #[test]
    fn a_vaults_state_is_never_definitions_and_rides_effects_routing() {
        // Seal and unseal move only this process's hold on the master key —
        // the engine routes them as reads, and the classifier rides that
        // decision rather than inventing a second one. The passphrase change
        // rewrites the vault's row and is a write.
        assert_eq!(
            family_of("SEAL VAULT team;"),
            PoolFamily::Reads,
            "a seal changes the process's hold, not the store"
        );
        assert_eq!(
            family_of("UNSEAL VAULT team WITH 'secret';"),
            PoolFamily::Reads
        );
        assert_eq!(
            family_of("CHANGE VAULT team PASSPHRASE FROM 'old' TO 'new';"),
            PoolFamily::Writes
        );
        for statement in [
            "SEAL VAULT team;",
            "UNSEAL VAULT team WITH 'secret';",
            "CHANGE VAULT team PASSPHRASE FROM 'old' TO 'new';",
        ] {
            assert_ne!(
                family_of(statement),
                PoolFamily::Definitions,
                "vault state was classed as a definition: {statement}"
            );
        }
    }
}
