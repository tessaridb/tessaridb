//! Opening a store, and the roles it opens with.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tessari_encoding::Roles;
use tessari_kv::KvBackend;

use crate::error::Result;
use crate::followers::Followers;
use crate::snapshots::Registry;

use super::{
    Store, give_an_older_log_its_home, give_an_older_log_its_writer, read_format_version,
    seed_version_position, write_initial_metadata,
};

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
        match read_format_version(Arc::clone(&backend))? {
            Some(found) => {
                found.check_supported()?;
                give_an_older_log_its_home(Arc::clone(&backend), found)?;
                give_an_older_log_its_writer(Arc::clone(&backend), found)?;
            }
            None => write_initial_metadata(Arc::clone(&backend))?,
        }
        seed_version_position(Arc::clone(&backend))?;
        crate::node::ensure(Arc::clone(&backend))?;
        let served = Arc::new(crate::served::Served::load(backend.as_ref())?);
        let expiring = Arc::new(crate::lapse::Expiring::load(backend.as_ref())?);
        let writing = Arc::new(crate::gate::WriteGate::default());
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
            decoded_tables: Arc::new(crate::catalog::DecodedTables::default()),
            catalog_rows: Arc::new(crate::catalog::CatalogRows::default()),
            served,
            expiring,
            public_appends: Arc::new(crate::topic::PublicRates::default()),
            divergences: Arc::new(AtomicU64::new(0)),
            discarded: Arc::new(AtomicU64::new(0)),
            campaigns: Arc::new(AtomicU64::new(0)),
            tally: Arc::default(),
            sampled: Arc::default(),
            retention: crate::retention::ProcessRetention::shared(),
            log_holds: crate::log_holds::LogHolds::shared(),
            followers: Arc::new(Followers::default()),
            holds: Arc::new(crate::holds::Holds::default()),
            collections: Arc::new(crate::collections::Collections::default()),
            lease: Arc::new(crate::lease::Held::default()),
            leading: Arc::new(std::sync::Mutex::new(None)),
            lines: Arc::new(crate::lines::Lines::default()),
            tailmarks: Arc::new(crate::tailmarks::TailMarks::default()),
            writing,
            decisions: Arc::default(),
        };
        // Last, because it reads the catalog: the format is settled and the
        // identity exists by the time this asks which node it is.
        store.reconcile_roles()?;
        // Rows are held only for readers at or above this: history before the
        // open is not known to have left them unchanged.
        store.catalog_rows.changed(store.committed_version()?);
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
}
