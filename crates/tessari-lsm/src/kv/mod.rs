//! The persistent backend on the TessariKV engine.
//!
//! A native implementation of [`AsyncKvBackend`]: every call is the engine's own future, so
//! nothing here blocks the thread that polls it and nothing here creates a runtime. Keyspaces
//! become named engine keyspaces, resolved once at open; batches become one engine write.
//!
//! # How a precondition is trusted without a lock
//!
//! A batch without preconditions is one atomic engine write and needs nothing else. A batch
//! with preconditions runs as a serializable engine transaction: it reads every precondition
//! key, compares, stages its writes and commits. The commit is the batch's linearization point,
//! and the engine refuses it when any key the transaction read or wrote was written by a commit
//! after its snapshot, so what the preconditions read is still true where the batch lands. A
//! refused commit runs the batch again from a fresh snapshot; a precondition that no longer holds
//! then answers [`Error::Conflict`] with nothing written. A retry happens only because another
//! commit landed, so some caller always makes progress (lock-free), and no lock is held at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tessari_kv::{
    AsyncKvBackend, Error, Key, KeyRange, Keyspace, KvFuture, Result, ScanRequest, Value,
    WriteBatch,
};
use tessari_vault::AtRestKey;
use tessarikv::{Db, DbOptions, KeyProvider, KeyspaceId, RootKey};

use crate::error::BACKEND_NAME;
use crate::options::keyspace_options;
use crate::{Durability, StoreConfig, encryption};

#[cfg(test)]
mod cache_tests;
mod closing;
mod errors;
mod reads;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod tests;
mod writes;

use errors::from_engine;

/// The key id every encrypted store is opened under; the store holds one key.
const KEY_ID: u32 = 1;

/// A durable [`AsyncKvBackend`] on the TessariKV engine.
pub struct TessariKvBackend {
    /// The engine; taken when the store closes, or handed to a background close when it is
    /// dropped open.
    db: Option<Db>,
    /// The runtime the store was opened on, which the engine's background work runs on.
    runtime: tokio::runtime::Handle,
    /// The engine keyspace of each of [`Keyspace::ALL`], in that order.
    keyspaces: Vec<KeyspaceId>,
    durability: Durability,
    path: PathBuf,
}

impl std::fmt::Debug for TessariKvBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TessariKvBackend")
            .field("path", &self.path)
            .field("durability", &self.durability.name())
            .finish_non_exhaustive()
    }
}

impl Drop for TessariKvBackend {
    /// A store dropped open is closed on the runtime it was opened on, so its background work
    /// finishes, synced, before an open in this process takes the directory.
    fn drop(&mut self) {
        if let Some(db) = self.db.take() {
            closing::close_in_background(&self.runtime, &self.path, db);
        }
    }
}

/// The store's one key, handed to the engine when it opens a file.
struct StoreKey([u8; tessarikv::ROOT_KEY_BYTES]);

impl KeyProvider for StoreKey {
    fn root_key(&self, key_id: u32) -> Option<RootKey> {
        (key_id == KEY_ID).then(|| RootKey::new(self.0))
    }
}

impl TessariKvBackend {
    /// Opens the store at `path`, creating it and any missing keyspace.
    ///
    /// # Errors
    ///
    /// The engine's failure to open, in the substrate's categories.
    pub async fn open(path: impl AsRef<Path>, config: StoreConfig) -> Result<Self> {
        Self::open_with_key(path, config, None).await
    }

    /// [`Self::open`], encrypted at rest under `key` when one is given.
    ///
    /// A store is encrypted from creation or never: an encrypted store opens only with its key,
    /// and a key is refused for a store created without one.
    ///
    /// # Errors
    ///
    /// The marker's refusals ([`encryption::admit`]) and the engine's failure to open.
    pub async fn open_with_key(
        path: impl AsRef<Path>,
        config: StoreConfig,
        key: Option<&AtRestKey>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // A store this process dropped at this directory is still closing: wait for it.
        closing::settled(&path).await;
        refuse_an_earlier_engine(&path)?;
        // The engine's manifest pointer is there exactly when a store is.
        let exists = path.join("CURRENT").exists();
        encryption::admit(&path, key, exists)?;
        let db = Db::open(&path, database_options(&config, key, exists)?)
            .await
            .map_err(|error| from_engine(&error))?;
        let keyspaces = Keyspace::ALL
            .iter()
            .map(|keyspace| {
                db.keyspace(keyspace.name())
                    .ok_or_else(|| Error::UnknownKeyspace {
                        keyspace: keyspace.name().to_owned(),
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Unavailable {
            backend: BACKEND_NAME,
            reason: "a store opens inside a tokio runtime".to_owned(),
        })?;
        Ok(Self {
            db: Some(db),
            runtime,
            keyspaces,
            durability: config.durability,
            path,
        })
    }

    /// The level this store makes a commit durable to.
    #[must_use]
    pub const fn durability(&self) -> Durability {
        self.durability
    }

    /// Compacts every keyspace whole.
    ///
    /// # Errors
    ///
    /// The engine's failure, after its background work stopped.
    pub async fn compact(&self) -> Result<()> {
        for id in &self.keyspaces {
            self.db()?
                .compact_range(*id, &[], None)
                .await
                .map_err(|error| from_engine(&error))?;
        }
        Ok(())
    }

    /// Closes the store: every acknowledged write is synced and the lock released.
    ///
    /// # Errors
    ///
    /// The engine's failure to finish its log or a flush.
    pub async fn close(mut self) -> Result<()> {
        match self.db.take() {
            Some(db) => db.close().await.map_err(|error| from_engine(&error)),
            None => Ok(()),
        }
    }

    /// The engine, while the store is open.
    fn db(&self) -> Result<&Db> {
        self.db.as_ref().ok_or_else(|| Error::Lifecycle {
            backend: BACKEND_NAME,
            reason: "the store is closed".to_owned(),
        })
    }

    /// The engine keyspace `keyspace` names.
    fn keyspace(&self, keyspace: Keyspace) -> Result<KeyspaceId> {
        Keyspace::ALL
            .iter()
            .position(|known| *known == keyspace)
            .and_then(|at| self.keyspaces.get(at).copied())
            .ok_or_else(|| Error::UnknownKeyspace {
                keyspace: keyspace.name().to_owned(),
            })
    }
}

/// How many digits this engine's manifest names carry; the engine before it wrote six.
const MANIFEST_DIGITS: usize = 20;

/// Refuses a store written by the storage engine before this one, touching nothing in it.
///
/// Its files are not this engine's, and opening them in place would answer as a damaged store.
/// What carries such a store over is a snapshot: taken by the build that wrote it, restored by
/// this one into a new store.
fn refuse_an_earlier_engine(path: &Path) -> Result<()> {
    let Ok(current) = std::fs::read_to_string(path.join("CURRENT")) else {
        return Ok(());
    };
    let named = current.trim();
    let digits = named.strip_prefix("MANIFEST-").map_or(0, str::len);
    if digits == 0 || digits == MANIFEST_DIGITS {
        return Ok(());
    }
    Err(Error::validation(format!(
        "the store at {} was written by an earlier storage engine and this build cannot open it \
         in place; take a snapshot with the build that wrote it (`tessaridb <store> --backup \
         <file>`) and restore it into a new store with this one (`tessaridb <new store> \
         --restore <file>`)",
        path.display()
    )))
}

/// The engine's options for `config`, encrypted under `key` when one is given.
///
/// A new store is created with every keyspace. An existing store missing one is refused rather
/// than extended in place: it was written by something with another idea of what a store holds,
/// and a keyspace added now would answer empty reads that look like a store with no data.
fn database_options(
    config: &StoreConfig,
    key: Option<&AtRestKey>,
    exists: bool,
) -> Result<DbOptions> {
    let keyspaces = u64::try_from(Keyspace::ALL.len()).unwrap_or(u64::MAX);
    let mut builder = DbOptions::builder()
        .create_if_missing(true)
        // Every keyspace is named, so a store from this build reopens.
        .create_missing_keyspaces(!exists)
        .serializable_transactions(true)
        .statistics(true)
        .block_cache_bytes(widen(config.block_cache_bytes))
        // Every keyspace's table, and the engine's own default one, fit at once.
        .total_memtable_bytes(
            widen(config.memtable_bytes).saturating_mul(keyspaces.saturating_add(1)),
        )
        .max_open_files(u32::try_from(config.max_open_files).unwrap_or(u32::MAX));
    for keyspace in Keyspace::ALL {
        let options = keyspace_options(*keyspace, widen(config.memtable_bytes))
            .map_err(|error| from_engine(&error))?;
        builder = builder.keyspace(keyspace.name(), options);
    }
    if let Some(key) = key {
        builder = builder.encryption(Arc::new(StoreKey(*key.engine().expose())), KEY_ID);
    }
    builder.build().map_err(|error| from_engine(&error))
}

/// A byte count as the engine counts them.
fn widen(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

impl AsyncKvBackend for TessariKvBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        Box::pin(self.read_one(keyspace, key))
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(self.read_range(request, true))
    }

    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(self.read_range(request, false))
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        Box::pin(self.read_firsts(keyspace, ranges))
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        Box::pin(self.read_count(keyspace, range))
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(self.land(batch, self.durability.syncs()))
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(self.land(batch, false))
    }

    fn sync_applied(&self) -> KvFuture<'_, ()> {
        Box::pin(async move {
            self.db()?
                .sync_wal()
                .await
                .map_err(|error| from_engine(&error))
        })
    }

    fn apply_group(
        &self,
        batches: Vec<WriteBatch>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = (usize, Result<()>)> + Send + '_>> {
        Box::pin(self.land_group(batches))
    }

    fn groups_writes(&self) -> bool {
        self.durability.syncs()
    }

    fn background_errors(&self) -> KvFuture<'_, u64> {
        // The engine stops at its first background failure, so the count is that one or none.
        Box::pin(async move { Ok(u64::from(self.db()?.background_error().is_some())) })
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        Box::pin(async move { Ok(self.read_one(keyspace, key).await?.is_some()) })
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        Box::pin(self.remove_range(keyspace, range))
    }
}
