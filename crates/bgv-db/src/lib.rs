//! bgv-db, embedded.
//!
//! One type to open a database, one to run a script against it, and the
//! vocabulary those two speak in. Everything else in this workspace is how the
//! store is built rather than how it is used, and is deliberately not reachable
//! from here.
//!
//! ```
//! use bgv_db::{Db, Value};
//!
//! let db = Db::in_memory()?;
//! let mut session = db.session();
//! session.run(
//!     "DEFINE NAMESPACE prod;
//!      USE NAMESPACE prod;
//!      DEFINE DATABASE orders;
//!      USE DATABASE orders;
//!      DEFINE TABLE users;
//!      CREATE users:1 = { name: 'ada' };",
//! )?;
//!
//! let found = session.run("SELECT name FROM users:1;")?;
//! let records = found[0].records().expect("a read answers with records");
//! assert_eq!(records.len(), 1);
//! # Ok::<(), bgv_db::Error>(())
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

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_lsm::LsmBackend;
use bgv_db_storage::{Catalog, Store};

pub use bgv_db_lsm::{Durability, StoreConfig};
pub use bgv_db_session::{AccessPath, Error, Outcome, Result, Session};
pub use bgv_db_storage::{Change, ChangeKind, Changes, Subscription, Watch};
pub use bgv_db_types::{
    Datetime, Duration, FieldKind, Number, Path as FieldPath, RecordId, RecordRef, Sequence, Step,
    TableId, Value,
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
        let backend = LsmBackend::open(path, config).map_err(bgv_db_storage::Error::from)?;
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
