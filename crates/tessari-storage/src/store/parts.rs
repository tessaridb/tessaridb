//! The store's parts, its transactions and the snapshots they hold.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tessari_encoding::{NodeIdentity, Roles};
use tessari_kv::KvBackend;
use tessari_types::{ConflictPolicy, Sequence, TableId};

use crate::catalog::Reach;
use crate::error::{Error, Result};
use crate::snapshots::Registry;
use crate::transaction::Transaction;

use super::Store;

impl Store {
    /// Who this node is.
    ///
    /// Generated once into the `META` keyspace and stable across restarts, so an
    /// id that changed would be a session token rather than an identity. It is
    /// not in the log and therefore not in a backup — a restore onto a fresh
    /// store produces a different node, which is the whole point of the split
    /// (ADR-0018 §1).
    ///
    /// # Read every time, and deliberately not cached
    ///
    /// This used to be resolved once at open, on the reasoning that it never
    /// changes while the store is open. `DEFINE NODE` makes that false, and a
    /// cache that is *usually* right is worse here than no cache: the last one
    /// produced a restore test that compared two handles and passed while the
    /// bytes on disk were wrong, because the value being asserted on had been
    /// read before the restore ran. The identity is small, the read is rare —
    /// `$node` and `INFO FOR NODE` are administrative — and one source of truth
    /// costs less than a second one that must be kept in step.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, [`Error::NoIdentity`] when the key has
    /// gone, or a decoding failure when the stored bytes carry a revision, role
    /// or membership this build does not know.
    pub fn node_identity(&self) -> Result<NodeIdentity> {
        crate::node::read(&self.backend)?.ok_or(Error::NoIdentity)
    }

    /// What this process is doing with the consumers the catalog declares.
    ///
    /// Empty until the runner starts something, and empty again after a restart
    /// — nothing here is persisted, because a persisted `running` flag outlives
    /// the thread it describes and the next process reads it as true.
    #[must_use]
    pub fn running(&self) -> &Arc<crate::running::Running> {
        &self.running
    }

    /// Where a read of a vault is recorded before its answer leaves.
    ///
    /// Handing this out is safe in a way handing out a key is not: what a caller
    /// can do with it is add a device that must also succeed for a read to be
    /// served. There is no way through it to make a read unrecorded.
    #[must_use]
    pub fn audit(&self) -> &Arc<crate::audit::AuditTrail> {
        &self.audit
    }

    /// Whether `count` more anonymous messages may be appended to the `PUBLIC`
    /// topic `topic` now, under `rule`, spending them from its allowance when
    /// they may. Counted per node and in memory (G037).
    #[must_use]
    pub fn admit_public_append(
        &self,
        topic: TableId,
        rule: crate::catalog::PublicAppend,
        count: u64,
    ) -> bool {
        self.public_appends
            .admit(topic, rule, count, std::time::Instant::now())
    }

    /// Which tables carry a retention floor.
    pub(crate) fn expiring(&self) -> &crate::lapse::Expiring {
        &self.expiring
    }

    pub(crate) fn series(&self) -> &Arc<crate::series::SeriesRegistry> {
        &self.series
    }

    /// Table definitions already decoded (`crate::catalog::decoded`).
    pub(crate) fn decoded_tables(&self) -> &crate::catalog::DecodedTables {
        &self.decoded_tables
    }

    /// Name and table rows held between statements (`crate::catalog::rows`).
    pub(crate) fn catalog_rows(&self) -> &crate::catalog::CatalogRows {
        &self.catalog_rows
    }

    /// Which tables are split, and where.
    pub(crate) fn shards(&self) -> &Arc<crate::shards::ShardRegistry> {
        &self.shards
    }

    /// What this node was last served under.
    pub(crate) fn served_state(&self) -> &Arc<crate::served::Served> {
        &self.served
    }

    /// Whether this process can open what the store's vaults hold.
    ///
    /// Sealed after every restart, deliberately: unsealing is the one thing
    /// nobody can automate away without also removing the property that makes a
    /// restart safe.
    #[must_use]
    pub fn vault(&self) -> &Arc<crate::vault::OpenVault> {
        &self.vault
    }

    /// Change what this node is for, and where it is reached.
    ///
    /// Absent arguments leave their field alone. Applied to the `META` keyspace
    /// immediately rather than through the transaction the statement runs in —
    /// the shape `BACKUP` already has, and for the same reason: `META` is not the
    /// log, so a write here cannot be part of a log transaction and pretending
    /// otherwise would be a durability claim the substrate does not support.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoIdentity`] when the store holds none, and the
    /// substrate's failure when the write is refused.
    pub fn configure_node(
        &self,
        roles: Option<Roles>,
        endpoints: Option<Vec<String>>,
        retain: Option<Option<Sequence>>,
    ) -> Result<NodeIdentity> {
        // Two keys rather than one record, because retention is not part of who
        // this node is: the identity carries a revision byte and refuses a
        // revision it does not know, so folding a disk budget into it would make
        // every store written by a newer build unreadable by an older one for a
        // number neither of them needs to agree on.
        if let Some(keep) = retain {
            self.set_log_retention(keep)?;
        }
        crate::node::configure(&self.backend, roles, endpoints)
    }

    /// Begin a transaction at the newest version this store has written.
    ///
    /// The newest **version**, not the newest log position: a snapshot is a
    /// statement about this store's own visible history, which is the fact the
    /// version counter holds (see [`Self::committed_version`]).
    ///
    /// # Errors
    ///
    /// Returns an error when the version position cannot be read or decoded.
    pub fn begin(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new(self, self.committed_version()?))
    }

    /// Begin a transaction reading the store as it stood at `at`.
    ///
    /// Records are versioned by a suffix on their own key, so reading the past
    /// is the read this store already performs with a different sequence — not
    /// a second mechanism. What has to be added is the honesty about when it
    /// cannot be done.
    ///
    /// Two refusals, and they are refusals rather than best-effort answers
    /// because both alternatives are a plausible wrong number that nothing
    /// reports:
    ///
    /// - **Below the reclaim floor.** Reclamation removed the versions that
    ///   would have answered, so the read would resolve to something older, or
    ///   to nothing, and call that the past.
    /// - **Above the newest version written.** There is no state there yet.
    ///   Answering with the present would make a read of the future silently
    ///   succeed and then change its answer the next time it is asked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VersionReclaimed`] when `at` is below the reclaim floor,
    /// [`Error::VersionInTheFuture`] when it is above the newest version
    /// written, or a backend error when either bound cannot be read.
    pub fn begin_at(&self, at: Sequence) -> Result<Transaction<'_>> {
        let floor = self.reclaim_floor()?;
        if at < floor {
            return Err(Error::VersionReclaimed {
                asked: at.get(),
                floor: floor.get(),
            });
        }
        let tail = self.committed_version()?;
        if at > tail {
            return Err(Error::VersionInTheFuture {
                asked: at.get(),
                tail: tail.get(),
            });
        }
        Ok(Transaction::new(self, at))
    }

    /// The oldest sequence any live reader can still need.
    ///
    /// Versions strictly older than the newest version at or below this may be
    /// reclaimed; nothing at or above it may be. With no reader live the floor is
    /// the newest version written, because a transaction that begins next will
    /// begin there.
    ///
    /// # Errors
    ///
    /// Returns an error when the version position cannot be read, which is only
    /// consulted when no snapshot is live.
    pub fn retention_floor(&self) -> Result<Sequence> {
        match self.snapshots.oldest() {
            Some(oldest) => Ok(oldest),
            None => self.committed_version(),
        }
    }

    /// How long the oldest live snapshot has been held, if one is.
    ///
    /// ADR-0005 §9 calls snapshot lifetime an operational limit rather than an
    /// application detail, because a long-held snapshot postpones every tombstone
    /// in the store. This is the value that limit is checked against.
    #[must_use]
    pub fn oldest_snapshot_age(&self) -> Option<std::time::Duration> {
        self.snapshots.oldest_age()
    }

    /// How many distinct snapshots are being read from.
    #[must_use]
    pub fn live_snapshots(&self) -> usize {
        self.snapshots.len()
    }

    /// The registry a transaction registers itself with.
    pub(crate) fn snapshot_registry(&self) -> &Arc<Registry> {
        &self.snapshots
    }

    /// Record that a declared last-writer-wins discarded writes.
    ///
    /// Called from the commit path and by nothing else. A store method rather
    /// than a counter in the serving process for the reason [`Store::campaigned`]
    /// is one: the scrape reads the store's health, so a detector that lives
    /// anywhere else is a detector an operator cannot see.
    pub(crate) fn discarded(&self, writes: u64) {
        self.discarded.fetch_add(writes, Ordering::Relaxed);
    }

    /// What a table does with a write it cannot order (G027 S3.2).
    ///
    /// **Silence is refusal**, deliberately and not as a fallback — it is what
    /// ADR-0075 has every table do and what every table written before the
    /// clause existed has always had done for it. A policy stored by a later
    /// build that this one cannot read is a decoding failure from the catalog
    /// and propagates as one, rather than being read as either answer.
    ///
    /// A table that is gone while a write to it is still in flight answers
    /// refusal for the same reason [`Store::admits_two_writers`] answers `false`
    /// on a missing namespace: an absent declaration is not a declaration, and
    /// of the two readings it is the one that loses nothing.
    pub(crate) fn conflict_policy(&self, table: TableId) -> Result<ConflictPolicy> {
        let mut transaction = self.begin()?;
        let Some(definition) = crate::catalog::Catalog::new(&mut transaction).table(table)? else {
            return Ok(ConflictPolicy::Refuse);
        };
        Ok(definition.conflict.unwrap_or(ConflictPolicy::Refuse))
    }

    /// Whether the range this log belongs to was **declared** multi-master
    /// (G027 S2.1).
    ///
    /// The declaration lives on the namespace, where ADR-0060 already put the
    /// replication clause and where G025 materialised the routing answer for a
    /// range. A log homed at the store answers `false`: the store log carries
    /// catalog records that every subscriber reads, and there is no namespace
    /// above it to have declared anything.
    ///
    /// **Silence is single-leader**, deliberately and not as a fallback — it is
    /// what this engine has always done, and what the refusal above has always
    /// enforced. A class stored by a later build that this one cannot read is a
    /// decoding failure from the catalog and propagates as one, rather than
    /// being read as either answer.
    pub(crate) fn admits_two_writers(&self, home: Reach) -> Result<bool> {
        let (Some(namespace), _) = home.parts() else {
            return Ok(false);
        };
        let mut transaction = self.begin()?;
        let Some(definition) =
            crate::catalog::Catalog::new(&mut transaction).namespace(namespace)?
        else {
            // The namespace is gone while its log is still being applied. Not a
            // declaration, so not an exemption — the fence stands, which is the
            // safe answer of the two.
            return Ok(false);
        };
        Ok(definition
            .class
            .is_some_and(tessari_types::ReplicationClass::admits_two_writers))
    }

    /// The backend, for the transaction's read and commit paths.
    pub(crate) fn backend(&self) -> &Arc<dyn KvBackend> {
        &self.backend
    }

    /// The turn a writer of a log record takes (`crate::gate`).
    pub(crate) fn write_gate(&self) -> &crate::gate::WriteGate {
        &self.writing
    }

    /// Call `hook` after every write that files a log record lands — a commit,
    /// or a record applied from another writer's stream — whichever surface,
    /// cadence or peer it came from.
    ///
    /// For whatever follows the log: it is how a follower learns there is
    /// something new without looking. The hook runs on the writer's thread
    /// after the write is readable, so it must be short and must not block.
    pub fn when_landed(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.writing.when_landed(Box::new(hook));
    }
}
