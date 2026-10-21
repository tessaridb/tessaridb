//! Opening a store, and the roles it opens with.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tessari_kv::KvBackend;

use tessari_encoding::Roles;

use crate::error::Result;
use crate::followers::Followers;
use crate::snapshots::Registry;

use super::{
    Store, give_an_older_log_its_home, give_an_older_log_its_writer, read_format_version,
    refuse_data_without_a_stamp, seed_version_position, write_initial_metadata,
};

impl Store {
    /// Close the store: its backend finishes its background work and releases
    /// what it holds, the directory included, before this answers.
    ///
    /// A store that is only dropped is closed in the background, and a process
    /// that exits first leaves the files a kill would have left.
    ///
    /// # Errors
    ///
    /// A lifecycle refusal when another handle to this store is still alive (a
    /// clone, or a reader holding the backend); the store stays open for it.
    pub async fn close(self) -> Result<()> {
        let backend = Arc::clone(&self.backend);
        drop(self);
        Ok(backend.close().await?)
    }

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
    pub async fn open(backend: Arc<dyn KvBackend>) -> Result<Self> {
        // The format is settled before anything else is written, the node
        // identity included: a store this build is about to refuse must not be
        // modified on the way to refusing it.
        match read_format_version(Arc::clone(&backend)).await? {
            Some(found) => {
                found.check_supported()?;
                give_an_older_log_its_home(Arc::clone(&backend), found).await?;
                give_an_older_log_its_writer(Arc::clone(&backend), found).await?;
            }
            None => {
                refuse_data_without_a_stamp(backend.as_ref()).await?;
                write_initial_metadata(Arc::clone(&backend)).await?;
            }
        }
        seed_version_position(Arc::clone(&backend)).await?;
        crate::node::ensure(Arc::clone(&backend)).await?;
        let served = Arc::new(crate::served::Served::load(backend.as_ref()).await?);
        let expiring = Arc::new(crate::lapse::Expiring::load(backend.as_ref()).await?);
        let writing = Arc::new(crate::gate::WriteGate::default());
        // Beneath the staged view, so the turn's holder sees staged writes of a
        // held space as it sees every other staged write (ADR-0130).
        let held = Arc::new(crate::memory::HeldSpaces::default());
        let routed = Arc::new(crate::memory::Routed::new(backend, Arc::clone(&held)));
        let backend: Arc<dyn KvBackend> = routed;
        let store = Self {
            // Every read the turn's holder makes sees what is staged behind it;
            // every other read sees the engine alone (`crate::gate`). Only a
            // backend that lands writes in groups has anything staged, so one
            // that does not is read directly and pays nothing for the view.
            backend: if backend.groups_writes() {
                Arc::new(crate::gate::Overlaid::new(backend, Arc::clone(&writing)))
            } else {
                backend
            },
            snapshots: Arc::new(Registry::default()),
            running: Arc::new(crate::running::Running::default()),
            // Sealed. A store that opened unsealed would be one that opens
            // secrets for whoever restarted it.
            vault: Arc::new(crate::vault::OpenVault::sealed()),
            attempts: Arc::new(crate::attempts::Attempts::new()),
            audit: Arc::new(crate::audit::AuditTrail::default()),
            series: Arc::new(crate::series::SeriesRegistry::default()),
            shards: Arc::new(crate::shards::ShardRegistry::default()),
            allocated: Arc::new(crate::catalog::Allocated::default()),
            decoded_tables: Arc::new(crate::catalog::DecodedTables::default()),
            catalog_rows: Arc::new(crate::catalog::CatalogRows::default()),
            served,
            expiring,
            held,
            public_appends: Arc::new(crate::topic::PublicRates::default()),
            partition_turns: Arc::new(crate::topic::PartitionTurns::default()),
            divergences: Arc::new(AtomicU64::new(0)),
            discarded: Arc::new(AtomicU64::new(0)),
            campaigns: Arc::new(AtomicU64::new(0)),
            tally: Arc::default(),
            sampled: Arc::default(),
            retention: crate::retention::ProcessRetention::shared(),
            log_holds: crate::log_holds::LogHolds::shared(),
            followers: Arc::new(Followers::default()),
            holds: Arc::new(crate::holds::Holds::default()),
            claiming: Arc::default(),
            collections: Arc::new(crate::collections::Collections::default()),
            lease: Arc::new(crate::lease::Held::default()),
            leading: Arc::new(std::sync::Mutex::new(None)),
            lines: Arc::new(crate::lines::Lines::default()),
            tailmarks: Arc::new(crate::tailmarks::TailMarks::default()),
            writing,
            decisions: Arc::default(),
        };
        // Before the first transaction begins: everything written so far landed
        // before this open, and every landing from here publishes its own.
        store.landed_at(store.committed_version().await?);
        // Last, because it reads the catalog: the format is settled and the
        // identity exists by the time this asks which node it is.
        store.reconcile_roles().await?;
        // And after it for the same reason: a finalize this node's older build
        // applied left the record and not the stamp (ADR-0118).
        store.catch_up_with_the_finalized_format().await?;
        // After the stamp is settled: a store holding format 8 keeps retired
        // index entries from the first commit after this open (Q-S3-34).
        store.begin_keeping_retired_entries().await?;
        // Rows are held only for readers at or above this: history before the
        // open is not known to have left them unchanged.
        store.catalog_rows.changed(store.committed_version().await?);
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
    pub async fn effective_roles(&self) -> Result<Roles> {
        let identity = self.node_identity().await?;
        let adopted = identity.roles;
        if self.lease.spent().is_none() && !self.awaiting(&identity.id).await? {
            return Ok(adopted);
        }
        let without_writing = adopted.bits() & !Roles::WRITABLE.bits();
        Ok(Roles::from_bits(without_writing).unwrap_or(Roles::NONE))
    }
}
