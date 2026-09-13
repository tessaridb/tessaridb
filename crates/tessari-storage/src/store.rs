//! The store: the handle that owns the backend and the committed tail.
//!
//! One type owns the substrate handle for its lifetime, resolves the store's
//! on-disk format at open, and hands out transactions. Everything above it
//! speaks in records and sequences; nothing above it sees a key, a keyspace or
//! a batch.

use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tessari_encoding::{
    AppliedPositionKey, FormatVersion, FormatVersionKey, LogKey, LogRecord, NODE_ID_LEN,
    NodeIdentity, Roles, StoreKey, StoreValue,
};
use tessari_kv::{KeyRange, KvBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{Epoch, Sequence};

use crate::catalog::Reach;
use crate::error::{Error, Result};
use crate::feed::Changes;
use crate::followers::{FollowerLag, Followers};
use crate::snapshots::Registry;
use crate::transaction::Transaction;

/// What a store says about itself when asked.
///
/// Deliberately small. A store that reports everything it knows is a metrics
/// endpoint, which is a different thing with a different audience; this answers
/// the one question a load balancer and a pager both ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    /// Background failures the engine has recorded.
    ///
    /// Any at all is unwell. There is no threshold to tune, because a single
    /// background error means some flush or compaction did not happen, and a
    /// store that has stopped keeping its own promises is not less unwell for
    /// having stopped only once.
    pub background_errors: u64,
    /// The log position every committed write is at or below.
    ///
    /// Carried because "the process is up" and "the store is readable" are
    /// different claims and only the second one is useful.
    pub committed: Sequence,
    /// Log divergences this process has refused.
    ///
    /// Not persisted, for the reason `crate::running`'s header gives about
    /// anything else that describes a process rather than a store: a count that
    /// outlived the process that observed it would be a claim nobody can check.
    /// It is here rather than nowhere because a detector added after the first
    /// incident is a detector that was absent during it.
    pub log_divergences: u64,
    /// How long this node may still write under its lease, or `None` when it
    /// holds none.
    ///
    /// Here beside the divergence count and for the same reason: this is where
    /// the detectors live, and the concept names *lease remaining* as the
    /// split-brain signal because the dangerous state is exactly this reaching
    /// zero while writes are still being accepted. `None` says no lease was
    /// granted, which is the ordinary state of a store standing alone and is
    /// not a value of zero.
    pub lease_remaining: Option<std::time::Duration>,
}

impl Health {
    /// Whether anything is wrong.
    #[must_use]
    pub const fn is_well(&self) -> bool {
        self.background_errors == 0
    }

    /// What is wrong, for somebody reading it at three in the morning.
    #[must_use]
    pub fn complaint(&self) -> Option<String> {
        if self.is_well() {
            return None;
        }
        Some(format!(
            "{} background error(s): a flush or compaction has failed, so this store \
             is answering reads while it has stopped keeping them",
            self.background_errors
        ))
    }
}

/// A record store over a key-value backend.
///
/// Cloning a store shares one backend **and one snapshot registry**: two handles
/// to the same store are not two stores, and a floor computed from half the live
/// readers would reclaim versions the other half is still reading.
#[derive(Debug, Clone)]
pub struct Store {
    backend: Arc<dyn KvBackend>,
    snapshots: Arc<Registry>,
    /// What this process is doing with the declared consumers.
    ///
    /// Shared like the snapshot registry and for the same reason: a session
    /// answering `INFO FOR KAFKA CONSUMER` and the thread doing the consuming must be
    /// looking at one registry, not at two that agree until they do not.
    running: Arc<crate::running::Running>,
    /// Whether this process can open what its vaults hold.
    ///
    /// Shared for the same reason as the two registries above, and the
    /// consequence of getting it wrong is larger: two handles to one store with
    /// two keyrings means one connection unseals and the next one is still
    /// sealed, which reads as an intermittent authorization fault rather than
    /// as the design error it is.
    ///
    /// It is **per-process and never persisted**. The root record travels in
    /// the log because every node needs it; the unsealed master key travels
    /// nowhere, so a follower holding every byte of the leader's log holds
    /// nothing that opens a secret.
    vault: Arc<crate::vault::OpenVault>,
    /// Where a read of a vault is recorded before its answer leaves.
    ///
    /// Beside the vault rather than inside it: the trail outlives any one
    /// unsealing, and a sealed store still records the reads it refused.
    audit: Arc<crate::audit::AuditTrail>,
    /// Which tables carry a retention floor.
    ///
    /// Shared for the reason the registries above it are, and held in memory
    /// for the reason its own module states: the floor is asked on the hottest
    /// path there is, and a catalog read there would charge every table for a
    /// feature only a series has.
    series: Arc<crate::series::SeriesRegistry>,
    /// Log divergences refused since this process opened the store.
    ///
    /// Shared with every handle for the same reason the snapshot registry is:
    /// two handles to one store are not two stores, and a count split between
    /// them is a count nobody can read.
    divergences: Arc<AtomicU64>,
    /// What this process has given each follower, and when.
    ///
    /// Shared with every handle for the reason the registries above it are, and
    /// held in memory for the reason `crate::followers` gives in its own
    /// header: a follower's progress is a fact about a live relationship, and a
    /// persisted copy of it would outlive the relationship it describes.
    followers: Arc<Followers>,
    /// What this node has collected for itself, and when it was last level.
    ///
    /// Held in memory for the reason `crate::collections` gives in its own
    /// header, which is `crate::followers`' argument turned round: a restored
    /// process would publish a currency claim about a relationship it has not
    /// had for a week.
    collections: Arc<crate::collections::Collections>,
    /// The lease this process is writing under, if it was given one.
    ///
    /// Shared with every handle for the reason the registries above it are, and
    /// held in memory for a sharper one: a persisted fence is an unreplicated
    /// file asserting a cluster-wide fact, which is the split-brain ADR-0018 §1
    /// keeps out of `META`.
    lease: Arc<crate::lease::Held>,
    /// The epoch a majority granted this node, if one ever did.
    ///
    /// Beside the lease rather than inside it, because they answer different
    /// questions and `crate::lease` is deliberately about the fence alone: the
    /// lease says *until when this node may write*, the epoch says *which
    /// leadership it is writing under*. A greeting carries the second and a
    /// commit is refused by the first.
    ///
    /// `None` for a node that never won a round, which is every single-node
    /// store and every node before its first campaign — a different statement
    /// from epoch zero, exactly as `None` from the lease is a different
    /// statement from a spent one.
    leading: Arc<std::sync::Mutex<Option<Epoch>>>,
    /// When this leader's own log reached each position.
    ///
    /// Shared with every handle for the reason the registries above it are, and
    /// held in memory for the sharpest of their reasons: it is a timeline of
    /// `Instant`s, which have no meaning outside the process that took them.
    /// See `crate::tailmarks` for why a follower's copy has no age without it.
    tailmarks: Arc<crate::tailmarks::TailMarks>,
}

impl Store {
    /// Open a store on `backend`, creating its metadata if it is new.
    ///
    /// A store whose on-disk format is newer than this build understands is
    /// **refused**. Opening it anyway would write this build's format into it,
    /// which is not recoverable afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, when the stored metadata cannot
    /// be decoded, or when the on-disk format is newer than this build.
    pub fn open(backend: Arc<dyn KvBackend>) -> Result<Self> {
        // The format is settled before anything else is written, the node
        // identity included: a store this build is about to refuse must not be
        // modified on the way to refusing it.
        match read_format_version(&backend)? {
            Some(found) => found.check_supported()?,
            None => write_initial_metadata(&backend)?,
        }
        crate::node::ensure(&backend)?;
        let store = Self {
            backend,
            snapshots: Arc::new(Registry::default()),
            running: Arc::new(crate::running::Running::default()),
            // Sealed. A store that opened unsealed would be one that opens
            // secrets for whoever restarted it.
            vault: Arc::new(crate::vault::OpenVault::sealed()),
            audit: Arc::new(crate::audit::AuditTrail::default()),
            series: Arc::new(crate::series::SeriesRegistry::default()),
            divergences: Arc::new(AtomicU64::new(0)),
            followers: Arc::new(Followers::default()),
            collections: Arc::new(crate::collections::Collections::default()),
            lease: Arc::new(crate::lease::Held::default()),
            leading: Arc::new(std::sync::Mutex::new(None)),
            tailmarks: Arc::new(crate::tailmarks::TailMarks::default()),
        };
        // Last, because it reads the catalog: the format is settled and the
        // identity exists by the time this asks which node it is.
        store.reconcile_roles()?;
        Ok(store)
    }

    /// The roles this node is actually serving under, which is the adopted set
    /// as the lease leaves it.
    ///
    /// `04_concept.md` §6.1 says *effective role is the lease*, and until this
    /// existed the two disagreed in the one situation that matters: a node whose
    /// lease had lapsed refused every write and went on reporting `writable`.
    /// The behaviour was already the lease — the fence is at the head of
    /// [`crate::Transaction::settle`] — so what was missing was that the report
    /// said so.
    ///
    /// # The refusal stays where it is, and the report follows it
    ///
    /// It would be one line to feed this set into the write gate instead of
    /// letting the fence refuse, and it would be wrong twice. The caller would
    /// get *this node is not writable*, which reads as a role misconfiguration
    /// and blames the write, in place of a refusal that says the **cluster** is
    /// what is wrong, carries how long the fence has been shut, and is
    /// categorised `unavailable`. And a node that is not writable is a node
    /// whose writes get forwarded to the writable peer — which, on a leader
    /// whose lease has lapsed, is itself.
    ///
    /// So there is one derivation and the report is downstream of it: this reads
    /// the same [`Held::spent`] the fence reads, which makes *reports writable
    /// while refusing writes* unrepresentable rather than merely unlikely.
    ///
    /// # A node with no lease keeps everything it adopted
    ///
    /// `None` from the lease is not a spent lease. A store nobody granted
    /// leadership to is not a leader running out of it, and every single-node
    /// deployment reaches this function and leaves it unchanged.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when the node
    /// identity cannot be read.
    pub fn effective_roles(&self) -> Result<Roles> {
        let identity = self.node_identity()?;
        let adopted = identity.roles;
        if self.lease.spent().is_none() && !self.awaiting(&identity.id)? {
            return Ok(adopted);
        }
        let without_writing = adopted.bits() & !Roles::WRITABLE.bits();
        Ok(Roles::from_bits(without_writing).unwrap_or(Roles::NONE))
    }

    /// Whether this node is in a cluster and holds no leadership yet.
    ///
    /// The second half of *the effective role is the lease*, and it was missing
    /// until ADR-0064. The first half only ever **subtracted**: a node whose
    /// lease lapsed stopped being writable. But [`crate::lease::Held::spent`]
    /// answers `None` in two states that are not alike — *this lease is still
    /// open* and *this node was never given one* — so a node that had never won
    /// a round fell through to the adopted set and reported the `WRITABLE` the
    /// catalog declared.
    ///
    /// While exactly one node could stand that cost nothing: the only writable
    /// node was the only candidate. ADR-0063 widened the candidate set, and the
    /// only configuration in which a failover can produce a writer at all is one
    /// where every coordinating node is declared writable — at which point every
    /// one of them writes from the moment it opens, because none of them holds a
    /// lease and the two that lose a round never will. Three writers, and
    /// nothing anywhere in an error state.
    ///
    /// # Why the predicate is the catalog and not the role
    ///
    /// A store standing alone has no cluster to grant it anything, so a global
    /// *has no lease* rule would stop every existing single-node deployment
    /// accepting writes on the day it upgraded. That much is unchanged, and it
    /// is why there has to be a predicate at all.
    ///
    /// It used to be [`Roles::COORDINATING`], on the reasoning that
    /// [`Roles::ALONE`] is documented as *not `COORDINATING`, because there is
    /// nothing to coordinate with*, so the bit already drew the line between a
    /// member of a deciding set and a store on its own — one predicate, two
    /// rules that cannot drift apart. The line is drawn correctly and it
    /// answers the wrong question. Roles are a **set**: `SERVING | WRITABLE`
    /// without `COORDINATING` is a legal, ordinary declaration for a writable
    /// node that does not vote, and such a node standing in a cluster beside an
    /// elected leader was never fenced at all — the gate asked for a bit it does
    /// not carry, so it wrote freely and silently beside somebody else's
    /// leadership.
    ///
    /// *May this node take part in deciding* (ADR-0063's `stands`) and *could
    /// there be a leader other than me* are two questions, and only the first is
    /// about the role. The second is about the catalog:
    /// [`Self::in_a_cluster`], which is `names_a_peer` over the committed
    /// membership rows — already this engine's one spelling of it, and already
    /// the bound a joiner follows.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when the node
    /// identity cannot be read.
    pub fn awaiting_leadership(&self) -> Result<bool> {
        self.awaiting(&self.node_identity()?.id)
    }

    /// Whether this node holds no leadership and is not alone in holding none.
    ///
    /// Takes the identity rather than reading it, so the two public callers pay
    /// for one node read between them instead of one each.
    ///
    /// # The lease is asked first because it is free
    ///
    /// [`crate::lease::Held::remaining`] is an in-memory read and the catalog is
    /// not, so a node that holds a leadership never reaches the second question
    /// — which is the state a leader is in for every commit it takes.
    fn awaiting(&self, me: &[u8; NODE_ID_LEN]) -> Result<bool> {
        if self.lease.remaining().is_some() {
            return Ok(false);
        }
        self.in_a_cluster(me)
    }

    /// Whether the committed catalog names a peer that is not this node.
    ///
    /// [`crate::names_a_peer`] over [`crate::Catalog::replicas`], and the
    /// definition carries the reasoning for both halves: why this is not *is the
    /// catalog empty*, and why the write gate asks this rather than asking what
    /// role the node was given.
    ///
    /// # Committed, deliberately, and it is the difference between joining and
    /// being unable to
    ///
    /// This opens its own transaction rather than reading through the one that
    /// is committing. A transaction sees its **own** pending writes, so the
    /// statement that declares the very first peer would find that peer while
    /// being judged, become clustered mid-commit, and refuse itself — leaving a
    /// standalone store with no way to join anything. A membership row that has
    /// not committed has not joined a cluster, so the committed state is also
    /// the answer that is true.
    ///
    /// Once it commits the node IS clustered and cannot write again until a
    /// round grants it something. That is the criterion, not a side effect.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when a stored
    /// membership row cannot be read.
    fn in_a_cluster(&self, me: &[u8; NODE_ID_LEN]) -> Result<bool> {
        let mut transaction = self.begin()?;
        let declared = crate::catalog::Catalog::new(&mut transaction).replicas()?;
        Ok(crate::catalog::names_a_peer(&declared, me))
    }

    /// How current this node's copy is known to be, or `None` when that cannot
    /// be established.
    ///
    /// A staleness bound is a promise about age, and `05_blocking-decisions.md`
    /// §C-05 decided that routing **excludes** a node beyond the bound rather
    /// than serving it with a marker — *a marker nobody is obliged to read is
    /// not a guarantee*. Excluding needs an age to compare, and this is the only
    /// one this build can honestly produce.
    ///
    /// # Currency here is an identity, not a measurement
    ///
    /// A node whose effective roles carry `writable` is the origin of the data
    /// it holds: there is nothing for it to be stale *relative to*, so its copy
    /// is current as of now. Reading [`Self::effective_roles`] rather than the
    /// adopted set is what makes that keep being true — a leader whose lease has
    /// lapsed stops being current in the same instant it stops being writable,
    /// which is the answer that is true, because from that moment somebody else
    /// may be taking writes it has not seen.
    ///
    /// # Unknown is outside every bound
    ///
    /// A node that may not write holds a copy of somebody else's writes, and
    /// nothing in this build can say how old that copy is. There is no follower
    /// loop: `Session::replicate_from` is the door on the **leader's** side and
    /// no part of this process pulls through it, so a collected copy has no last
    /// collection to be measured from. `None` is therefore the honest answer and
    /// a caller must treat it as beyond every bound — which refuses something
    /// that might have been fine rather than serving something that might not
    /// be, the same direction [`crate::Lease`] errs.
    ///
    /// # Unknown until it has been level, and that is not the same as unknown
    /// until it has collected
    ///
    /// A node that may not write now answers the time since it was last
    /// **level** with the peer it collects from — see [`crate::Collections`] for
    /// why a collection that filled its bound proves only that this node asked.
    /// A node that has collected and never arrived still answers `None`, because
    /// its copy has no age anybody can state.
    ///
    /// It is the age of the last *arrival* and not of the data, so it is a lower
    /// bound: the leader may have written since. That is the same caveat
    /// [`crate::FollowerLag::quiet_for`] carries on the other side, and closing
    /// it needs a time in the log, which is Q-542's.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when the node
    /// identity cannot be read.
    pub fn current_as_of(&self) -> Result<Option<std::time::Duration>> {
        if self.effective_roles()?.has(Roles::WRITABLE) {
            return Ok(Some(std::time::Duration::ZERO));
        }
        Ok(self
            .collections
            .last()
            .and_then(|collection| collection.level_at)
            .map(|level| level.elapsed()))
    }

    /// Adopt the role the cluster wants this node to have.
    ///
    /// The **effective** role of `04_concept.md` §6.1 moving toward the
    /// **desired** one. A membership row bound to this node's id says what it is
    /// supposed to be; `META` says what it currently is; this closes the gap.
    ///
    /// Answers the roles it adopted, or `None` when it adopted nothing — which
    /// is both of the ordinary cases: no row names this node, or one does and
    /// the two already agree.
    ///
    /// # Why here, and why only here
    ///
    /// *The panel assigns, the node reconciles* (§C-19). Opening the store is
    /// the node's own reconcile point: it is the moment the process has a
    /// catalog to read and has not yet answered anybody, so the role it serves
    /// under is the role it settled on rather than one that changed underneath a
    /// request. A node converging **while running** would need a watch over the
    /// replicated row, and nothing watches it: the peer cadences pull records
    /// and exchange greetings, but no path re-adopts a role after open. So the
    /// window in which desired and effective differ is, for now, exactly the
    /// span between a declaration and the next open. That window is the thing
    /// S5.2 asks to be observable, and it is.
    ///
    /// It follows that `DEFINE NODE ROLES` on a **bound** node is an override
    /// the next open discards. That is C-19's decision showing through rather
    /// than an accident: a node's role is shared truth, and a local word that
    /// outlived the shared one would be the split-brain this whole section
    /// exists to prevent, in miniature.
    ///
    /// # A bound row with no roles drains the node
    ///
    /// An absent `ROLES` clause already means [`Roles::NONE`], which already
    /// means *takes no writes* — and `Roles::NONE` is documented as how an
    /// operator drains a node without stopping it. Applied to this node the same
    /// value keeps the same meaning, so binding a row and saying nothing about
    /// roles drains it at the next open. That is a sharp edge and it is the
    /// price of one value meaning one thing; the alternative is a second
    /// spelling for absent, and two spellings for absent disagree.
    ///
    /// # A membership row this build cannot read now refuses the open
    ///
    /// New, and deliberate. Before this, an unreadable row broke `INFO FOR NODE`
    /// and a forward; now it stops the store opening at all, because the
    /// question it makes unanswerable is *what is this node allowed to be*. The
    /// two ways to be wrong are not symmetric: refusing is an outage an operator
    /// sees immediately, and carrying on means running under a role the cluster
    /// may not have given — which is a node accepting writes it was supposed to
    /// forward, silently, which is the failure `roles` exists to prevent.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when a stored
    /// membership row or the node identity cannot be read.
    fn reconcile_roles(&self) -> Result<Option<Roles>> {
        let identity = self.node_identity()?;
        let mut transaction = self.begin()?;
        let desired = crate::catalog::Catalog::new(&mut transaction).desired_roles(&identity.id)?;
        let Some(desired) = desired else {
            return Ok(None);
        };
        if desired == identity.roles {
            return Ok(None);
        }
        self.configure_node(Some(desired), None)?;
        Ok(Some(desired))
    }

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

    /// Which tables carry a retention floor.
    pub(crate) fn series(&self) -> &Arc<crate::series::SeriesRegistry> {
        &self.series
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
    ) -> Result<NodeIdentity> {
        crate::node::configure(&self.backend, roles, endpoints)
    }

    /// Begin a transaction at the current committed tail.
    ///
    /// # Errors
    ///
    /// Returns an error when the committed tail cannot be read or decoded.
    pub fn begin(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new(self, self.committed_tail()?))
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
    /// - **Above the committed tail.** There is no state there yet. Answering
    ///   with the present would make a read of the future silently succeed and
    ///   then change its answer the next time it is asked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VersionReclaimed`] when `at` is below the reclaim floor,
    /// [`Error::VersionInTheFuture`] when it is above the committed tail, or a
    /// backend error when either bound cannot be read.
    pub fn begin_at(&self, at: Sequence) -> Result<Transaction<'_>> {
        let floor = self.reclaim_floor()?;
        if at < floor {
            return Err(Error::VersionReclaimed {
                asked: at.get(),
                floor: floor.get(),
            });
        }
        let tail = self.committed_tail()?;
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
    /// the committed tail, because a transaction that begins next will begin
    /// there.
    ///
    /// # Errors
    ///
    /// Returns an error when the committed tail cannot be read, which is only
    /// consulted when no snapshot is live.
    pub fn retention_floor(&self) -> Result<Sequence> {
        match self.snapshots.oldest() {
            Some(oldest) => Ok(oldest),
            None => self.committed_tail(),
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

    /// Whether this store is well, and what is wrong when it is not.
    ///
    /// # Why this exists rather than a metric
    ///
    /// An engine does its compaction, its flushing and its write-ahead work on
    /// its own threads, and a failure there surfaces at **no call a caller
    /// makes**. The store keeps answering reads while the thing that keeps it
    /// durable has stopped. That is the one failure this store cannot detect by
    /// being used, so something has to ask.
    ///
    /// # Where the alert lives, and why it is not here
    ///
    /// Not here. This answers *what is true*; deciding it is worth waking
    /// somebody for belongs to whatever already wakes people. The HTTP surface
    /// turns an unwell store into a failing `GET /health`, which every load
    /// balancer takes out of rotation and every monitor pages on — so the alert
    /// is the one that already exists rather than a second one written here and
    /// tested never.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the counts cannot be read.
    pub fn health(&self) -> Result<Health> {
        Ok(Health {
            background_errors: self.backend.background_errors()?,
            committed: self.committed_tail()?,
            log_divergences: self.divergences.load(Ordering::Relaxed),
            lease_remaining: self.lease.remaining(),
        })
    }

    /// Record what a follower has been given.
    ///
    /// # Why the store holds this and not the door
    ///
    /// The door — `Session::replicate_from` — is where a follower names itself
    /// and where the read happens, so it is where the call is made. But the
    /// registry belongs to the store, because every handle to one store is one
    /// leader: a follower recorded against a second handle is a follower the
    /// first one would report as never having collected.
    pub fn follower_served(&self, node: [u8; NODE_ID_LEN], reached: Sequence) {
        self.followers.served(node, reached);
    }

    /// Record what this node collected for itself, and whether it arrived.
    ///
    /// The follower's twin of [`Self::follower_served`], and the only way
    /// [`Self::current_as_of`] ever answers anything but `None` on a node that
    /// may not write. `currency` is the caller's observation and not a
    /// judgement: a collection whose answer was shorter than the bound it named
    /// is [`Currency::Level`], because a peer serves `min(limit, available)` and
    /// a short answer means it had no more.
    pub fn collected(&self, reached: Sequence, currency: crate::collections::Currency) {
        self.collections.collected(reached, currency);
    }

    /// The last collection this node made for itself, if it has made one.
    #[must_use]
    pub fn collection(&self) -> Option<crate::collections::Collection> {
        self.collections.last()
    }

    /// How far behind every follower this process has served is.
    ///
    /// Measured against this leader's own committed tail, from what it handed
    /// out — no connection to the follower is opened, and none is needed,
    /// because the leader served every byte the follower holds.
    ///
    /// A follower that has never collected is **absent** from this list rather
    /// than present at zero. `INFO FOR NODE` draws the same distinction one
    /// level up between a node no membership row names and one whose row names
    /// no roles.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the committed tail cannot be read.
    pub fn follower_lag(&self) -> Result<Vec<FollowerLag>> {
        let tail = self.committed_tail()?;
        Ok(self
            .followers
            .seen()
            .into_iter()
            .map(|(node, held)| FollowerLag {
                node,
                sequence: held.sequence,
                behind: tail.get().saturating_sub(held.sequence.get()),
                quiet_for: held.at.elapsed(),
                copy_age: self.tailmarks.age_of(held.sequence),
            })
            .collect())
    }

    /// Date this leader's own committed tail, as of now.
    ///
    /// Called once per awareness interval by the node binary, and by nothing
    /// else. It is what gives [`crate::FollowerLag::copy_age`] a timeline to be
    /// read against; a leader that never calls it reports every follower's copy
    /// age as unknown, which is the honest answer for a leader that has never
    /// dated anything.
    ///
    /// Deliberately **not** on the commit path. Sampling where the tail actually
    /// moves would be exact and would put a lock on the hottest path in the
    /// engine for the sake of a diagnostic; the cadence already runs and already
    /// opens the store. What that costs is precision, bounded at one interval
    /// and always in the safe direction — see [`crate::tailmarks`].
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the committed tail cannot be read.
    pub fn mark_tail(&self) -> Result<()> {
        self.tailmarks.mark(self.committed_tail()?);
        Ok(())
    }

    /// Take or renew the lease this process writes under.
    ///
    /// The fence closes `LEASE_GUARD` before the lease expires, so this node
    /// stops writing strictly before the cluster is entitled to give the
    /// leadership to somebody else. See [`crate::lease`] for why the two
    /// instants are deliberately not the same one.
    ///
    /// Nothing in this build calls it but a test: granting a lease is a cluster
    /// act and needs a wire. What exists here is the fence.
    pub fn hold_lease(&self, ttl: std::time::Duration) {
        self.lease.take(ttl);
    }

    /// Hold a lease a majority granted, exactly as it was granted.
    ///
    /// The seam between the cluster and the engine, and the reason it takes a
    /// whole [`Lease`] rather than a span: a granted lease is dated from the
    /// instant its round **opened**, and a duration arriving here cannot carry
    /// that instant — it would restart the clock at the moment of installation,
    /// so every millisecond the round spent collecting would come out of the
    /// **voters'** window instead of this node's. That is the split-brain the
    /// dating rule exists to prevent, reached through the seam rather than
    /// through the rule.
    ///
    /// Nothing is re-checked here. Whether the grant was legitimate was settled
    /// by the round; a store asking again would be asking about a fact it has no
    /// way to know.
    pub fn hold(&self, epoch: Epoch, lease: crate::lease::Lease) {
        self.lease.hold(lease);
        if let Ok(mut leading) = self.leading.lock() {
            *leading = Some(epoch);
        }
    }

    /// The leadership epoch this node is writing under, if a round granted it
    /// one.
    ///
    /// A greeting says *the leadership it believes is current*, and until this
    /// existed the only honest answer a serving node could give was the constant
    /// zero — true while nothing campaigned, and a lie to every peer the moment
    /// something did.
    ///
    /// `None` is not zero. A node nobody elected is not leading under the first
    /// epoch; it is not leading at all, and a caller that wants the constant for
    /// a store that never campaigns can say so in one word at its own call site.
    #[must_use]
    pub fn leading(&self) -> Option<Epoch> {
        match self.leading.lock() {
            Ok(leading) => *leading,
            // The fence's decision, for the fence's reason: a diagnostic that
            // fails open leaves a gap in a report, and this one feeds a greeting
            // a peer routes on. Saying nothing is the conservative answer, and
            // it is the one a node that never campaigned gives anyway.
            Err(_) => None,
        }
    }

    /// How long this node's lease fence has been closed, if it is.
    ///
    /// `None` means writes may proceed — either because the fence is still open
    /// or because this node was never given a lease at all. A node nobody
    /// granted leadership to is not a leader running out of it.
    #[must_use]
    pub fn lease_spent(&self) -> Option<std::time::Duration> {
        self.lease.spent()
    }

    /// The highest sequence that has been committed.
    ///
    /// While a commit and its application are the same event — which they are
    /// until the replication log separates them — the committed tail *is* the
    /// applied position, so no second key exists for it.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be read or decoded.
    pub fn committed_tail(&self) -> Result<Sequence> {
        let key = AppliedPositionKey.encode();
        let stored = self.backend.get(AppliedPositionKey::keyspace(), &key)?;
        match stored {
            Some(value) => Ok(Sequence::decode(value.as_slice())?),
            None => Ok(Sequence::ZERO),
        }
    }

    /// The leadership under which the newest record this node holds was written.
    ///
    /// Raft's `lastLogTerm`, and it is a different fact from [`Self::leading`].
    /// `leading` says *which epoch a majority granted THIS node*, which is
    /// `None` on a follower that has never campaigned however much history it
    /// holds. This says *which leadership wrote the last thing here*, which is
    /// what an election restriction has to compare: a follower carrying the
    /// newest records must not read as behind a node that once led and then
    /// fell away. Ordering a candidate by the wrong one of the two inverts the
    /// answer exactly where it matters.
    ///
    /// [`Epoch::ZERO`] for an empty log, which is the same value a store that
    /// has elected nobody holds — so the first record of a fresh log needs no
    /// special case at any call site, and it is the convention
    /// `refuse_a_parted_history` already uses one position further back.
    ///
    /// Costs one point read and a fixed eight-byte inspection: the epoch sits at
    /// a known offset in the stored record and the mutations are never decoded.
    ///
    /// # Errors
    ///
    /// Returns an error when the tail cannot be read, or when the record stored
    /// at it cannot be inspected — which is corruption rather than an absence.
    pub fn tail_leadership(&self) -> Result<Epoch> {
        let tail = self.committed_tail()?;
        if tail == Sequence::ZERO {
            return Ok(Epoch::ZERO);
        }
        let stored = self
            .backend
            .get(LogKey::keyspace(), &LogKey::new(tail).encode())?;
        // A tail naming a record the log does not hold is the retention case
        // Q-529 owns, and the honest answer here is the same one
        // `refuse_a_parted_history` gives: nothing to compare against. A node
        // that cannot state the leadership of its own tail is treated as
        // holding none, which makes it lose every comparison rather than win
        // one it cannot support.
        let Some(value) = stored else {
            return Ok(Epoch::ZERO);
        };
        Ok(LogRecord::epoch_in(value.as_slice())?)
    }

    /// Read log records from `from` onward, oldest first.
    ///
    /// `limit` bounds the read because a log is unbounded by nature and a caller
    /// that asks for "the rest of it" is asking for however much has accumulated
    /// since it last looked.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    /// The record changes from `from` onward, oldest first.
    ///
    /// A projection of [`Store::log_records`] and nothing more: the feed holds no
    /// state, cannot disagree with what was committed, and is identical on a
    /// replica reading the same log. Catalog changes are not in it — a
    /// subscriber watching `users` did not ask for the rows that describe
    /// `users` — and a change says what a record *became* rather than whether it
    /// is new; both are explained in [`crate::feed`].
    ///
    /// `limit` bounds the **log records** read, not the changes produced, so one
    /// commit is never returned half-way: a subscriber applies a commit as the
    /// unit it was written as. For the same reason the answer carries the
    /// position to resume from — a commit that only touched the catalog yields
    /// no changes, and a reader given only a list could not tell that from
    /// "nothing has happened" and would ask for the same records forever.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, or when a record or a payload
    /// cannot be decoded. A payload that cannot be decoded is corruption rather
    /// than a change to skip.
    pub fn changes_since(&self, from: Sequence, limit: usize) -> Result<Changes> {
        let records = self.log_records(from, limit)?;
        let next = records.last().map_or(from, |(sequence, _)| {
            Sequence::new(sequence.get().saturating_add(1))
        });
        let mut changes = Vec::new();
        for (sequence, record) in records {
            changes.extend(crate::feed::changes_in(sequence, &record)?);
        }
        Ok(Changes { changes, next })
    }

    /// Read log records from `from` onward, oldest first.
    ///
    /// `limit` bounds the read because a log is unbounded by nature and a caller
    /// that asks for "the rest of it" is asking for however much has accumulated
    /// since it last looked.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    pub fn log_records(&self, from: Sequence, limit: usize) -> Result<Vec<(Sequence, LogRecord)>> {
        let prefix = LogKey::prefix();
        let bounds = KeyRange::prefix(&prefix);
        let request = ScanRequest {
            keyspace: LogKey::keyspace(),
            range: KeyRange::from_bounds(
                Bound::Included(LogKey::new(from).encode()),
                bounds.end().clone(),
            ),
            direction: ScanDirection::Forward,
            limit: Some(limit),
        };
        self.backend
            .scan(&request)?
            .into_iter()
            .map(|(key, value)| {
                let sequence = LogKey::decode(key.as_slice())?.sequence;
                let record = LogRecord::decode(value.as_slice())?;
                Ok((sequence, record))
            })
            .collect()
    }

    /// Log records for a subscriber, carrying only what its reach reaches.
    ///
    /// # Every sequence arrives, and that is the whole of the design
    ///
    /// The leader does not skip a record it filtered to nothing — it delivers it
    /// empty. A follower's position check compares the epoch of the record
    /// **before** the one it is offered (ADR-0059), so a record that simply
    /// vanished from the numbering would read as a parted history and refuse the
    /// stream. Delivering it empty costs a frame and keeps the arithmetic the
    /// gap rule already does: [`Self::apply_record`] advances `committed_tail`
    /// over a record with no mutations exactly as it does over a full one.
    ///
    /// The cost is stated rather than discovered: a selective follower's stream
    /// is O(all commits) in **frames** while being O(its own commits) in bytes.
    /// Bounded by commits rather than by data.
    ///
    /// # The filter is on the leader, deliberately
    ///
    /// A follower could be sent everything and asked to keep what it is entitled
    /// to. That is a confidentiality model in which the party being restricted
    /// is the one applying the restriction, and it is the arrangement this store
    /// refuses everywhere else. The epoch is preserved across the rebuild
    /// because it identifies the leadership that wrote the commit, not its
    /// contents.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Self::log_records`] returns, and
    /// [`Error::CatalogMalformed`] when a catalog record in the log is present
    /// and cannot be decoded.
    pub fn log_records_within(
        &self,
        subscription: Reach,
        from: Sequence,
        limit: usize,
    ) -> Result<Vec<(Sequence, LogRecord)>> {
        if subscription == Reach::Store {
            // Not an optimisation with a caveat — a statement. A store-reach
            // subscription receives the log unchanged, so rebuilding every
            // record to arrive at the same bytes would cost a clone per mutation
            // on the path every follower that exists today takes, to prove
            // something the type already says.
            return self.log_records(from, limit);
        }
        let mut carried = Vec::new();
        for (sequence, record) in self.log_records(from, limit)? {
            let mut kept = Vec::new();
            for mutation in record.mutations() {
                if crate::catalog::carried_to(mutation)?.reaches(subscription) {
                    kept.push(mutation.clone());
                }
            }
            carried.push((sequence, LogRecord::at(record.epoch(), kept)));
        }
        Ok(carried)
    }

    /// Apply a record that arrived from a peer, which claims what stands
    /// before it.
    ///
    /// [`Self::apply_record`] refuses a divergence **only where the two logs
    /// overlap** — a record offered at a position this store already holds. It
    /// cannot see the case where they do not. A sender whose history parted
    /// from this store's at sequence 4 offers sequence 6; that is `tail + 1`
    /// here, so there is nothing at the position to compare and the record is
    /// appended. This store then holds 1-5 from one history and 6 from another,
    /// with no error anywhere and both nodes reporting healthy — which is the
    /// failure ADR-0059 exists to remove, one position further back than the
    /// record half reached.
    ///
    /// So the sender states the epoch of the record **before** the one it is
    /// offering, and this compares that against what it actually holds there.
    /// It is Raft's `AppendEntries` consistency check, which reads the
    /// follower's own entry at `prevLogIndex` and compares its term — not the
    /// follower's current term, and not the leader's.
    ///
    /// The refusal names **`at - 1`**: the position where the histories part,
    /// rather than the one where the check happened to run. An operator reading
    /// it needs the first number to know where to re-bootstrap from.
    ///
    /// The predecessor of sequence 1 is [`Epoch::ZERO`], which is what a store
    /// that has elected nobody holds — so the first record of a fresh log needs
    /// no special case at the call site.
    ///
    /// # Why this is a separate method and not a parameter
    ///
    /// A local commit knows its own predecessor by construction and has nothing
    /// to claim; a record arriving from a peer carries a claim about a history
    /// this store may not share. Those are different acts. An
    /// `Option<Epoch>` on [`Self::apply_record`] would make *the local path*
    /// and *a peer that said nothing* the same shape, and the check would then
    /// be skippable by forgetting a field.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogDivergence`] when the predecessor this store holds
    /// was written under a different leadership, and whatever
    /// [`Self::apply_record`] returns otherwise.
    pub fn apply_from_stream(
        &self,
        at: Sequence,
        previous: Epoch,
        record: &LogRecord,
    ) -> Result<()> {
        self.refuse_a_parted_history(at, previous)?;
        self.apply_record(at, record)
    }

    /// Apply one log record, at the sequence it carries.
    ///
    /// This is what a replica runs, and it is the same function a commit runs
    /// once it has decided its sequence locally.
    ///
    /// Re-applying a record the store already holds is a **no-op**, not an
    /// error: a replica that is re-sent a record it already has has not been
    /// told anything wrong, and refusing would turn an ordinary retry into an
    /// incident. Skipping *forward* is refused, because a gap means the state
    /// would no longer be explained by any log.
    ///
    /// # A retry and a divergence arrive the same way
    ///
    /// Both land on a position this store already holds, and until the log
    /// record carried an epoch there was nothing to tell them apart — so the
    /// second writer's record was discarded in silence and two nodes diverged
    /// while both reported healthy (ADR-0059). The comparison is against the
    /// epoch held **at that sequence**, read from the log, and not against the
    /// store's latest epoch: a follower catching up legitimately replays records
    /// from leaderships that have since ended, and every one of them would be a
    /// false divergence against the latest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogDivergence`] when another leadership wrote this position,
    /// [`Error::LogGap`] when the record is not the next one, and the mapped
    /// backend or decoding failure otherwise.
    pub fn apply_record(&self, at: Sequence, record: &LogRecord) -> Result<()> {
        let applied = self.committed_tail()?;
        if at.get() <= applied.get() {
            self.refuse_a_divergence(at, record.epoch())?;
            return Ok(());
        }
        let expected = Sequence::new(applied.get().saturating_add(1));
        if at != expected {
            return Err(Error::LogGap {
                expected,
                found: at,
            });
        }
        // A replica re-checks what the leader already checked. That is cheap
        // relative to the apply, and a violation reaching this point is a
        // divergence between two nodes' catalogs rather than a caller's mistake
        // — which is worth stopping at rather than writing through.
        crate::schema::validate(self, record)?;
        let batch = crate::index::maintain(self, record, crate::log::apply_batch(at, record))?;
        // Derived here as well as in the commit, because that is the whole
        // reason it is derived from the record: a follower that skipped this
        // would carry the edges and no way to walk them, and its walks would
        // answer nothing while the leader answered correctly — the symptom
        // `crate::adjacency`'s own header names as the reason it derives from
        // the mutation at all. It skipped it anyway, from W148 until W185,
        // because the replay called two of these three and nothing compared a
        // replica that held an edge (Q-452).
        //
        // The order matches the commit path deliberately: two paths that build
        // one batch in two orders are a difference waiting to become a
        // divergence nobody can explain.
        let batch = crate::adjacency::maintain(self, record, batch)?;
        // Derived here as well as in the commit, because that is the whole
        // reason it is derived from the record: a follower that skipped this
        // would carry the records and none of the counts, and its planner would
        // then choose a different access path for the same query.
        let batch = crate::cardinality::maintain(self, record, batch, at)?;
        self.backend.apply(batch)?;
        Ok(())
    }

    /// Refuse a record from a leadership other than the one held at `at`.
    ///
    /// The vocabulary is the databases', not the chains': Kafka calls this log
    /// divergence and fixed it by putting a leader epoch in the log (KIP-101),
    /// PostgreSQL calls it a diverging timeline, MongoDB reaches the common
    /// point and rolls back. Only a leaderless design escapes it, by paying
    /// conflict resolution instead.
    ///
    /// Costs one point read and a fixed eight-byte inspection — the epoch sits
    /// in front of the mutations precisely so this does not decode the record —
    /// and it runs only on the branch a duplicate delivery takes.
    /// Refuse a record whose predecessor this store never wrote.
    ///
    /// The sibling of [`Self::refuse_a_divergence`], one position earlier. That
    /// one compares the record being offered against what stands at its own
    /// position; this one compares what the sender says stands **before** it
    /// against what actually does — which is the only way to catch a divergence
    /// that happened entirely behind this store's tail.
    ///
    /// Costs the same as its sibling: one point read and a fixed eight-byte
    /// inspection, no decode of the mutations.
    fn refuse_a_parted_history(&self, at: Sequence, previous: Epoch) -> Result<()> {
        let Some(before) = at.get().checked_sub(1) else {
            return Ok(());
        };
        let before = Sequence::new(before);
        if before == Sequence::ZERO {
            // Nothing precedes the first record, and a store that has elected
            // nobody holds `Epoch::ZERO` — so a sender claiming anything else
            // is describing a history this store does not have.
            if previous == Epoch::ZERO {
                return Ok(());
            }
            self.divergences.fetch_add(1, Ordering::Relaxed);
            return Err(Error::LogDivergence {
                sequence: before,
                held: Epoch::ZERO,
                offered: previous,
            });
        }
        let stored = self
            .backend
            .get(LogKey::keyspace(), &LogKey::new(before).encode())?;
        // Nothing to compare against — this store is behind the sender by more
        // than one record, and `apply_record` refuses that with `LogGap`, which
        // is the more accurate answer. The truncation case is Q-529's, the same
        // hole the position check carries and the same decision: it belongs with
        // retention, because there is one answer for both.
        let Some(value) = stored else {
            return Ok(());
        };
        let held = LogRecord::epoch_in(value.as_slice())?;
        if held == previous {
            return Ok(());
        }
        self.divergences.fetch_add(1, Ordering::Relaxed);
        Err(Error::LogDivergence {
            sequence: before,
            held,
            offered: previous,
        })
    }

    fn refuse_a_divergence(&self, at: Sequence, offered: Epoch) -> Result<()> {
        let stored = self
            .backend
            .get(LogKey::keyspace(), &LogKey::new(at).encode())?;
        // Nothing to compare against. Unreachable today because the log keyspace
        // is never truncated, and it becomes reachable the day retention reaches
        // it — at which point a node that was away long enough is exactly the
        // node this check was written for (Q-529).
        let Some(value) = stored else {
            return Ok(());
        };
        let held = LogRecord::epoch_in(value.as_slice())?;
        if held == offered {
            return Ok(());
        }
        self.divergences.fetch_add(1, Ordering::Relaxed);
        Err(Error::LogDivergence {
            sequence: at,
            held,
            offered,
        })
    }

    /// The backend, for the transaction's read and commit paths.
    pub(crate) fn backend(&self) -> &Arc<dyn KvBackend> {
        &self.backend
    }
}

/// The format this store was written in, if it has been written at all.
///
/// A free function rather than a method because it runs before the store
/// exists: `open` settles the format before it resolves the node identity, and
/// the identity is one of the store's own fields.
fn read_format_version(backend: &Arc<dyn KvBackend>) -> Result<Option<FormatVersion>> {
    let key = FormatVersionKey.encode();
    let stored = backend.get(FormatVersionKey::keyspace(), &key)?;
    match stored {
        Some(value) => Ok(Some(FormatVersion::decode(value.as_slice())?)),
        None => Ok(None),
    }
}

/// Write the metadata a fresh store needs, refusing if someone raced us.
///
/// The `Absent` precondition is what makes two processes opening the same
/// new store safe: exactly one of them writes the metadata.
fn write_initial_metadata(backend: &Arc<dyn KvBackend>) -> Result<()> {
    let format_key = FormatVersionKey.encode();
    let applied_key = AppliedPositionKey.encode();
    let batch = WriteBatch::new()
        .expect_absent(FormatVersionKey::keyspace(), format_key.clone())
        .put(
            FormatVersionKey::keyspace(),
            format_key,
            FormatVersion::CURRENT.encode(),
        )
        .put(
            AppliedPositionKey::keyspace(),
            applied_key,
            Sequence::ZERO.encode(),
        );
    backend.apply(batch)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_kv::MemoryBackend;

    use super::*;
    use crate::error::Error;

    fn backend() -> Arc<dyn KvBackend> {
        Arc::new(MemoryBackend::new())
    }

    #[test]
    fn a_fresh_store_writes_its_format_and_starts_at_sequence_zero() {
        let store = Store::open(backend()).unwrap();
        assert_eq!(store.committed_tail().unwrap(), Sequence::ZERO);
        assert_eq!(
            read_format_version(store.backend()).unwrap(),
            Some(FormatVersion::CURRENT)
        );
    }

    #[test]
    fn reopening_a_store_does_not_rewrite_its_metadata() {
        let shared = backend();
        let first = Store::open(Arc::clone(&shared)).unwrap();
        drop(first);
        let second = Store::open(shared).unwrap();
        assert_eq!(
            read_format_version(second.backend()).unwrap(),
            Some(FormatVersion::CURRENT)
        );
    }

    #[test]
    fn a_store_written_before_the_epoch_opens_and_is_not_rewritten() {
        // The half of the format move that matters operationally: version 2 must
        // not orphan the stores version 1 already wrote, and opening one must
        // not quietly upgrade it either — an upgrade on open is a one-way door
        // taken by a process that was only asked to read.
        let shared = backend();
        Store::open(Arc::clone(&shared)).unwrap();
        shared
            .apply(WriteBatch::new().put(
                FormatVersionKey::keyspace(),
                FormatVersionKey.encode(),
                FormatVersion::new(1).encode(),
            ))
            .unwrap();

        let store = Store::open(Arc::clone(&shared)).unwrap();
        assert_eq!(
            read_format_version(store.backend()).unwrap(),
            Some(FormatVersion::new(1)),
            "opening an older store left its recorded format alone"
        );

        store
            .apply_record(Sequence::new(1), &LogRecord::new(Vec::new()))
            .unwrap();
        let (_, record) = store
            .log_records(Sequence::new(1), 1)
            .unwrap()
            .pop()
            .expect("the record just applied");
        assert_eq!(
            record.epoch(),
            tessari_types::Epoch::ZERO,
            "a build that elects nobody writes the first and only leadership"
        );
    }

    #[test]
    fn a_newer_on_disk_format_is_refused_rather_than_opened() {
        let shared = backend();
        let future = FormatVersion::new(FormatVersion::CURRENT.get().saturating_add(1));
        shared
            .apply(WriteBatch::new().put(
                FormatVersionKey::keyspace(),
                FormatVersionKey.encode(),
                future.encode(),
            ))
            .unwrap();

        let error = Store::open(shared).unwrap_err();
        assert_eq!(error.code(), "incompatible");
        assert!(!error.is_retryable());
        match error {
            Error::Encoding(inner) => {
                assert!(inner.to_string().contains("format version"), "{inner}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
