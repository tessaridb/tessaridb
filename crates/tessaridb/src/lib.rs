//! TessariDB, embedded.
//!
//! One type to open a database, one to run a script against it, and the
//! vocabulary those two speak in. Everything else in this workspace is how the
//! store is built rather than how it is used, and is deliberately not reachable
//! from here.
//!
//! ```
//! use tessaridb::{Db, Value};
//!
//! let db = Db::in_memory()?;
//! let mut session = db.session();
//! session.run(
//!     "DEFINE NAMESPACE prod;
//!      USE NAMESPACE prod;
//!      DEFINE DATABASE orders;
//!      USE DATABASE orders;
//!      DEFINE COLLECTION users;
//!      CREATE users:1 = { name: 'ada' };",
//! )?;
//!
//! let found = session.run("SELECT name FROM users:1;")?;
//! let records = found[0].records().expect("a read answers with records");
//! assert_eq!(records.len(), 1);
//! # Ok::<(), tessaridb::Error>(())
//! ```
//!
//! # Two ways to open, one behaviour
//!
//! [`Db::in_memory`] and [`Db::open`] differ in where the bytes live and in
//! nothing else. That is not a convenience — it is the claim the whole storage
//! layer is built to support, and the conformance suite that runs against both
//! substrates is what makes it a claim rather than a hope.
//!
//! # Following what changes
//!
//! The change feed is a projection of the replication log rather than a
//! mechanism of its own, so it needs no setup and holds no state: a
//! [`Subscription`] is a value you keep, and its position is a number you can
//! store and come back with. See [`Db::changes_since`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_lsm::LsmBackend;
use tessari_storage::{Catalog, ReplicaDefinition, Roles, Store};

/// The value a piece of text denotes, when it denotes one by itself.
///
/// How a **supplied** value is written on every surface outside a script: the
/// CLI's `--param x=3`, and the `parameters` of an HTTP request body. TessariQL
/// rather than each surface's own notation, because there is one value syntax
/// here and the console already reads and writes it — what an answer prints
/// pastes back into the next statement, and `dec 12.34`, `2s` and
/// `datetime '…'` all say themselves.
///
/// Read **in isolation**, so it is a value or it is nothing: `1; DROP TABLE
/// users` is refused as a literal rather than smuggled in as a statement. A
/// read, a path or anything needing a record is likewise not a value here — an
/// argument that had to consult the store to say what it is would be a statement
/// wearing a value's clothes.
///
/// It lives in the facade because two surfaces need it and neither is above the
/// other (ADR-0012).
///
/// # Errors
///
/// Returns a sentence naming what the text is instead of a value.
pub fn value_of(written: &str) -> core::result::Result<tessari_types::Value, String> {
    let refusal = || format!("{written:?} is not a value TessariQL can read on its own");
    match tessari_ql::parse_expression(written)
        .map_err(|_| refusal())?
        .kind
    {
        tessari_ql::ExprKind::Literal(value) => Ok(value),
        _ => Err(refusal()),
    }
}

pub mod feed;

pub use tessari_lsm::{Durability, StoreConfig};
pub use tessari_session::redact::{Visible, seen};
pub use tessari_session::{
    AccessPath, Error, Exactness, Nearest, Note, Outcome, Parameters, Result, Session, Suggestion,
    Ticket,
};
pub use tessari_storage::{BUILD_VERSION, Change, ChangeKind, Changes, Lease, Subscription, Watch};
pub use tessari_types::{
    DatabaseId, Datetime, Duration, FieldKind, Geometry, NamespaceId, Number, Path as FieldPath,
    Polygon, Position, RecordId, RecordRef, Ring, Sequence, Step, TableId, Value, from_geojson,
    geojson_name, to_geojson,
};

/// Every table an answer's references point at.
///
/// Walked rather than assumed: a reference can be anywhere in a record — in a
/// field, inside an array, nested in an object — and a walk that stopped at the
/// top level would render the common shapes and miss the interesting ones.
fn referenced(value: &Value, into: &mut BTreeSet<TableId>) {
    match value {
        Value::Record(held) => {
            into.insert(held.table);
        }
        Value::Table(held) => {
            into.insert(*held);
        }
        Value::Array(items) => {
            for item in items {
                referenced(item, into);
            }
        }
        Value::Set(items) => {
            for item in items {
                referenced(item, into);
            }
        }
        Value::Object(fields) => {
            for held in fields.values() {
                referenced(held, into);
            }
        }
        _ => {}
    }
}

/// An open database.
///
/// Owns its store; a [`Session`] borrows from it. One process holds one `Db` and
/// opens as many sessions as it likes — which is the shape the layers below
/// already have, so the facade does not invent an owned session that would have
/// to reach through a lock to find the store again.
#[derive(Debug)]
pub struct Db {
    store: Store,
}

impl Db {
    /// Open a database that lives only as long as this process.
    ///
    /// The same store, the same language, the same guarantees as [`Db::open`] —
    /// everything except where the bytes are. Useful for a test suite, for a
    /// cache, and for finding out what a script does before running it against
    /// something that remembers.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be initialised.
    pub fn in_memory() -> Result<Self> {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        Ok(Self {
            store: Store::open(backend)?,
        })
    }

    /// Open a database at `path`, creating it if it is not there.
    ///
    /// Uses the storage profile's defaults. A caller who has measured something
    /// passes their own through [`Db::open_with`].
    ///
    /// # Errors
    ///
    /// Returns an error when the engine cannot open the path, or when the store
    /// cannot be initialised on it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, StoreConfig::default())
    }

    /// Open a database at `path` with a configuration you chose.
    ///
    /// # Errors
    ///
    /// Returns an error when the engine cannot open the path, or when the store
    /// cannot be initialised on it.
    pub fn open_with(path: impl AsRef<Path>, config: StoreConfig) -> Result<Self> {
        let backend = LsmBackend::open(path, config).map_err(tessari_storage::Error::from)?;
        let backend = Arc::new(backend) as Arc<dyn KvBackend>;
        Ok(Self {
            store: Store::open(backend)?,
        })
    }

    /// A session on this database, with nothing selected.
    ///
    /// A session carries the namespace and database a script has said `USE` for,
    /// and holds an open transaction between `BEGIN` and `COMMIT`. Two sessions
    /// on one database are two independent conversations with it.
    #[must_use]
    pub fn session(&self) -> Session<'_> {
        Session::new(&self.store)
    }

    /// Take or renew the lease this node writes under.
    ///
    /// The seam between the cluster and the engine: a candidate that carried a
    /// round to a majority of voting members tells the store how long that
    /// majority agreed it may write for, and the store closes its fence
    /// `LEASE_GUARD` before the span runs out. Nothing here asks who granted it
    /// — the round did that, and a store that re-checked would be checking a
    /// fact it has no way to know.
    ///
    /// A node nobody granted leadership to never calls this and is not fenced:
    /// it is not a leader running out of time.
    pub fn hold_lease(&self, ttl: core::time::Duration) {
        self.store.hold_lease(ttl);
    }

    /// Hold a lease a majority granted, exactly as it was granted.
    ///
    /// The form a cluster uses, and the difference from [`Db::hold_lease`] is
    /// the instant. A granted lease is dated from when its round **opened**, so
    /// a slow round yields a shorter window; a span arriving here instead would
    /// restart that clock on installation and spend the collection delay out of
    /// the voters' window rather than this node's.
    ///
    /// A node nobody granted leadership to never calls either form and is not
    /// fenced by one.
    ///
    /// The epoch travels with the lease because they were granted together and
    /// are read together: the fence answers *may I still write*, the epoch
    /// answers *which leadership am I writing under*, and a greeting carries the
    /// second to every peer that routes on it.
    pub fn hold(&self, epoch: tessari_types::Epoch, lease: Lease) {
        self.store.hold(epoch, lease);
    }

    /// The leadership epoch this node is writing under, if a round granted it
    /// one.
    #[must_use]
    pub fn leading(&self) -> Option<tessari_types::Epoch> {
        self.store.leading()
    }

    /// What changed from `from` onward, oldest first.
    ///
    /// A projection of the replication log: no state, no registration, and the
    /// same answer on a replica reading the same log. The result carries where
    /// to resume, because a commit that changed no records still moves a reader
    /// forward.
    ///
    /// # Errors
    ///
    /// Returns an error when a record or a payload cannot be read.
    pub fn changes_since(&self, from: Sequence, limit: usize) -> Result<Changes> {
        Ok(self.store.changes_since(from, limit)?)
    }

    /// A database over a store somebody else opened.
    ///
    /// The inverse of [`Db::store`], and it names nothing this type does not
    /// already name. Two callers want it: a test that needs a backend behaving
    /// in a way no ordinary one does, and an embedder who assembled the store
    /// themselves and wants the front door over it anyway.
    #[must_use]
    pub const fn from_store(store: Store) -> Self {
        Self { store }
    }

    /// The names of the tables an answer's record references point at.
    ///
    /// # Why an answer cannot be rendered without this
    ///
    /// A record reference holds a table **id** and a record id, because that is
    /// what the key grammar stores and what a reference has to be to survive a
    /// rename. The name the language writes — `users:1` — lives in the catalog.
    /// So a renderer handed a `Value` alone cannot produce a reference anybody
    /// can use: the console would print `1:2` and a JSON client would receive
    /// `"1:2"`, which is indistinguishable from a value it could follow and is
    /// not one.
    ///
    /// # Why it is here rather than in each surface
    ///
    /// Because there are two of them. The console and the HTTP endpoint both
    /// need this, and two implementations of "what is this table called" would
    /// eventually disagree about a renamed table — with one surface answering
    /// the old name and the other the new, which is worse than neither
    /// answering.
    ///
    /// # Why it walks the answer first
    ///
    /// The catalog is only read when the answer actually holds a reference, so a
    /// read of records that carry none costs a walk over values already in
    /// memory rather than a catalog scan. Most answers hold none.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn names_in(&self, records: &[(RecordId, Value)]) -> Result<BTreeMap<TableId, String>> {
        let mut wanted = BTreeSet::new();
        for (_, held) in records {
            referenced(held, &mut wanted);
        }
        let mut named = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(named);
        }
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        for table in wanted {
            // A reference to a table that has been dropped keeps its id and
            // gains no name, the same way a reference to a deleted record keeps
            // its id: the field still says what it says.
            if let Some(held) = catalog.table(table)? {
                named.insert(table, held.name);
            }
        }
        Ok(named)
    }

    /// What a table is called, for a caller holding only its id.
    ///
    /// The other direction of [`Db::names_in`], and here for the same reason: a
    /// change on the feed names its table by id, and an id means nothing to
    /// anybody outside this process. A table that has been dropped has no name
    /// and keeps its id, the way a reference to a deleted record does.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn table_name(&self, table: TableId) -> Result<Option<String>> {
        let mut transaction = self.store.begin()?;
        Ok(Catalog::new(&mut transaction)
            .table(table)?
            .map(|held| held.name))
    }

    /// Where the peer that takes writes answers, if one is declared.
    ///
    /// The forward's target (ADR-0019 §2, case *forward*). At v1 there is one
    /// range covering everything, so "the leader of the range this statement
    /// touches" and "the peer declared writable" are the same peer — which is
    /// what ADR-0019 §1 means by the same lookup serving both eras, and why this
    /// answers with the endpoint rather than with a range.
    ///
    /// `None` when nothing is declared writable. That is an answer, not a
    /// failure: a node that may not write and knows of nobody who may is
    /// correctly configured for a cluster of one that has been drained, and the
    /// caller says so rather than guessing at an address.
    ///
    /// **More than one is refused.** Two peers declared writable is the split
    /// brain the whole design is arranged to prevent, and picking either one —
    /// the first, the lowest id, the alphabetically smallest — would be a
    /// routing decision taken by a sort order.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read, and
    /// [`Error::ManyWritablePeers`] when more than one peer is declared
    /// writable.
    ///
    /// # Why the whole row and not the endpoint
    ///
    /// Two callers want this peer and they want different fields of it: a
    /// forwarded write needs somewhere to send the statement, and a follower
    /// collecting the log needs the node id as well, because a peer connection
    /// derives the name it demands of the peer's certificate from that id. A
    /// second finder for the second field would be a second answer to *which
    /// peer may write*, and the two would disagree the day somebody declared
    /// two writable peers and only one of them checked.
    pub fn writable_peer(&self) -> Result<Option<ReplicaDefinition>> {
        let mut transaction = self.store.begin()?;
        let mut writable = Catalog::new(&mut transaction)
            .replicas()?
            .into_iter()
            .filter(|peer| peer.roles.has(Roles::WRITABLE));
        let Some(found) = writable.next() else {
            return Ok(None);
        };
        if let Some(second) = writable.next() {
            return Err(Error::ManyWritablePeers {
                named: found.name,
                also: second.name,
            });
        }
        Ok(Some(found))
    }

    /// Resolve the namespace and database a session has selected.
    ///
    /// A caller that reads the log needs these, because the log is every
    /// tenancy's and a reader confined to one has to know which ids that is.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read. A name that is not
    /// there is `None`: not existing is an answer.
    pub fn tenancy_in(
        &self,
        namespace: &str,
        database: &str,
    ) -> Result<Option<(NamespaceId, DatabaseId)>> {
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let Some(namespace) = catalog.namespace_id(namespace)? else {
            return Ok(None);
        };
        Ok(catalog
            .database_id(namespace, database)?
            .map(|database| (namespace, database)))
    }

    /// Resolve a table by the three names a session selects it with.
    ///
    /// One method rather than three, because a caller that resolved a namespace
    /// and a database itself would be holding two ids it has no other use for,
    /// and every step of the walk is the same catalog read.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read. A name that is not
    /// there is `None` rather than an error: not existing is an answer.
    pub fn table_in(
        &self,
        namespace: &str,
        database: &str,
        table: &str,
    ) -> Result<Option<TableId>> {
        let mut transaction = self.store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let Some(namespace) = catalog.namespace_id(namespace)? else {
            return Ok(None);
        };
        let Some(database) = catalog.database_id(namespace, database)? else {
            return Ok(None);
        };
        Ok(catalog.table_id(namespace, database, table)?)
    }

    /// The store underneath.
    ///
    /// Exposed for the operations that are not statements. A backup is a read of
    /// the log rather than a query, and giving this facade one method per such
    /// tool would make it grow with the tools rather than with the database.
    #[must_use]
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// The position of the newest committed change.
    ///
    /// A subscription starting after this one sees only what happens next.
    ///
    /// # Errors
    ///
    /// Returns an error when the position cannot be read.
    pub fn committed_tail(&self) -> Result<Sequence> {
        Ok(self.store.committed_tail()?)
    }

    /// Follow the changes to one table, or to all of them, from `from` onward.
    ///
    /// The subscription is a value you keep. It holds a position and two
    /// counters and nothing else, so this database has no registry of
    /// subscribers: nothing to leak, nothing to clean up when a caller
    /// disappears, and no lock on the write path.
    #[must_use]
    pub const fn subscribe(from: Sequence, watch: Watch) -> Subscription {
        Subscription::new(from, watch)
    }

    /// The next changes a subscription is waiting for, advancing it.
    ///
    /// Advances over every record read, not only over the ones that matched, so
    /// a subscription watching one table does not stall on a run of writes to
    /// another.
    ///
    /// # Errors
    ///
    /// Returns an error when a record or a payload cannot be read.
    pub fn poll(&self, subscription: &mut Subscription, limit: usize) -> Result<Vec<Change>> {
        Ok(subscription.poll(&self.store, limit)?)
    }

    /// Give up on a subscription's backlog below `target`, counting the loss.
    ///
    /// For a subscriber that would rather be current than complete. Returns how
    /// many watched changes were discarded — exactly, because the skip reads
    /// what it discards, and an approximate loss figure is one nobody can act
    /// on.
    ///
    /// # Errors
    ///
    /// Returns an error when a record or a payload cannot be read.
    pub fn skip(&self, subscription: &mut Subscription, target: Sequence) -> Result<u64> {
        Ok(subscription.skip_to(&self.store, target)?)
    }
}
