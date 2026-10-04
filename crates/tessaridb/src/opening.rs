//! Opening a database: in memory, on disk, encrypted, or over a store already open.

use super::*;

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
        let backend: Arc<dyn KvBackend> = Arc::new(MemoryBackend::new());
        Ok(Self {
            store: Store::open(backend)?,
            gather: std::sync::OnceLock::new(),
            participants: std::sync::OnceLock::new(),
            standing_since: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            coordinate: std::sync::OnceLock::new(),
            elsewhere: std::sync::OnceLock::new(),
            budget: std::sync::OnceLock::new(),
            certificates: std::sync::OnceLock::new(),
            commits: std::sync::OnceLock::new(),
            backups: std::sync::OnceLock::new(),
            at_rest: None,
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
        Self::open_encrypted(path, config, None)
    }

    /// Open a database at `path`, its files encrypted under `key` when one is
    /// given (ADR-0108 D7) — and every backup it produces sealed under it.
    ///
    /// A store opens only the way it was created: an encrypted one refuses to
    /// open without its key or under another, and a plain one refuses a key.
    ///
    /// # Errors
    ///
    /// As [`Db::open_with`], and a refusal naming which key is wrong.
    pub fn open_encrypted(
        path: impl AsRef<Path>,
        config: StoreConfig,
        key: Option<AtRestKey>,
    ) -> Result<Self> {
        let backend = LsmBackend::open_with_key(path, config, key.as_ref())
            .map_err(tessari_storage::Error::from)?;
        let backend: Arc<dyn KvBackend> = Arc::new(backend);
        Ok(Self {
            store: Store::open(backend)?,
            gather: std::sync::OnceLock::new(),
            participants: std::sync::OnceLock::new(),
            standing_since: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            coordinate: std::sync::OnceLock::new(),
            elsewhere: std::sync::OnceLock::new(),
            budget: std::sync::OnceLock::new(),
            certificates: std::sync::OnceLock::new(),
            commits: std::sync::OnceLock::new(),
            backups: std::sync::OnceLock::new(),
            at_rest: key.map(Arc::new),
        })
    }

    /// A session on this database, with nothing selected.
    ///
    /// A session carries the namespace and database a script has said `USE` for,
    /// and holds an open transaction between `BEGIN` and `COMMIT`. Two sessions
    /// on one database are two independent conversations with it.
    #[must_use]
    pub fn session(&self) -> Session<'_> {
        let session = Session::new(&self.store);
        let session = match self.gather.get() {
            Some(gather) => session.gathering(Arc::clone(gather)),
            None => session,
        };
        let session = match self.elsewhere.get() {
            Some(known) => session.among(Arc::clone(known)),
            None => session,
        };
        let session = match self.participants.get() {
            Some(participants) => session.participating(Arc::clone(participants)),
            None => session,
        };
        let session = match self.budget.get() {
            Some(budget) => session.budgeted(Arc::clone(budget)),
            None => session,
        };
        let session = match self.certificates.get() {
            Some(shown) => session.presenting(Arc::clone(shown)),
            None => session,
        };
        let session = match &self.at_rest {
            Some(key) => session.sealing_backups(Arc::clone(key)),
            None => session,
        };
        match self.backups.get() {
            Some(folder) => session.backing_up_into(Arc::clone(folder)),
            None => session,
        }
    }

    /// The key this database's store is encrypted under and its backups are
    /// sealed with, when it has one.
    #[must_use]
    pub fn at_rest(&self) -> Option<&AtRestKey> {
        self.at_rest.as_deref()
    }

    /// A database over a store somebody else opened.
    ///
    /// The inverse of [`Db::store`], and it names nothing this type does not
    /// already name. Two callers want it: a test that needs a backend behaving
    /// in a way no ordinary one does, and an embedder who assembled the store
    /// themselves and wants the front door over it anyway.
    #[must_use]
    pub const fn from_store(store: Store) -> Self {
        Self {
            store,
            gather: std::sync::OnceLock::new(),
            participants: std::sync::OnceLock::new(),
            standing_since: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            coordinate: std::sync::OnceLock::new(),
            elsewhere: std::sync::OnceLock::new(),
            budget: std::sync::OnceLock::new(),
            certificates: std::sync::OnceLock::new(),
            commits: std::sync::OnceLock::new(),
            backups: std::sync::OnceLock::new(),
            at_rest: None,
        }
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
}
