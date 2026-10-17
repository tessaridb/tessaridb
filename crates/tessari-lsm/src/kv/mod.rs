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
use tessarikv::{Db, DbOptions, KeyProvider, KeyspaceId, KeyspaceOptions, RootKey};

use crate::error::BACKEND_NAME;
use crate::{Durability, StoreConfig, encryption};

mod errors;
mod reads;
#[cfg(test)]
mod tests;
mod writes;

use errors::from_engine;

/// The key id every encrypted store is opened under; the store holds one key.
const KEY_ID: u32 = 1;

/// A durable [`AsyncKvBackend`] on the TessariKV engine.
pub struct TessariKvBackend {
    db: Db,
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
        // The engine's manifest pointer is there exactly when a store is.
        encryption::admit(&path, key, path.join("CURRENT").exists())?;
        let db = Db::open(&path, database_options(&config, key)?)
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
        Ok(Self {
            db,
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
            self.db
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
    pub async fn close(self) -> Result<()> {
        self.db.close().await.map_err(|error| from_engine(&error))
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

/// The engine's options for `config`, encrypted under `key` when one is given.
fn database_options(config: &StoreConfig, key: Option<&AtRestKey>) -> Result<DbOptions> {
    let keyspaces = u64::try_from(Keyspace::ALL.len()).unwrap_or(u64::MAX);
    let keyspace_options = KeyspaceOptions::builder()
        .memtable_bytes(widen(config.memtable_bytes))
        .build()
        .map_err(|error| from_engine(&error))?;
    let mut builder = DbOptions::builder()
        .create_if_missing(true)
        // Every keyspace is named, so a store from this build reopens, and one missing a
        // keyspace gains it.
        .create_missing_keyspaces(true)
        .serializable_transactions(true)
        .block_cache_bytes(widen(config.block_cache_bytes))
        // Every keyspace's table, and the engine's own default one, fit at once.
        .total_memtable_bytes(
            widen(config.memtable_bytes).saturating_mul(keyspaces.saturating_add(1)),
        )
        .max_open_files(u32::try_from(config.max_open_files).unwrap_or(u32::MAX));
    for keyspace in Keyspace::ALL {
        builder = builder.keyspace(keyspace.name(), keyspace_options.clone());
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
            self.db
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
        Box::pin(async move { Ok(u64::from(self.db.background_error().is_some())) })
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        Box::pin(async move { Ok(self.read_one(keyspace, key).await?.is_some()) })
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        Box::pin(self.remove_range(keyspace, range))
    }
}
