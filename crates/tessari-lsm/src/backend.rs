//! The persistent backend.
//!
//! A durable implementation of [`KvBackend`] on a log-structured merge-tree
//! engine. Keyspaces become regions of the store; batches become one engine
//! batch; preconditions become a check taken under the write lock.
//!
//! # How a precondition can be trusted
//!
//! The engine's batch gives atomicity and nothing else. Two callers that each
//! read a value, decide, and write it in separate batches will still lose one
//! update, and both batches will have been perfectly atomic — atomicity answers
//! "can a reader see half of this", not "can two writers interleave".
//!
//! So the check and the write have to be one indivisible step, and there are
//! three ways to get there. A validating transaction re-checks at commit and
//! retries on conflict, which is the wrong shape here because the layer above
//! commits through a single shared position key: every commit would contend and
//! the retry loop would become the workload. A locking transaction buys a lock
//! manager, lock timeouts and deadlock detection to protect a store that the
//! engine already guarantees has one writer process.
//!
//! The third way is to have one writer. The engine holds an exclusive lock on
//! the store directory, so no second process can write; [`Self::apply`] is the
//! only path that mutates anything; so one lock held across the whole of it
//! makes check-then-write atomic, and reads taken inside it see the latest
//! committed state. That is what this backend does, and it is the same shape the
//! in-memory backend already had, which keeps one concurrency story across both.
//!
//! Commits therefore serialise. That was already true one layer up, where the
//! committed position is a single contended key, so it adds no new bottleneck.
//! When it does become one, the answer is one batching writer draining a queue —
//! not a weaker guarantee.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rocksdb::{ColumnFamily, DB, IteratorMode, Options, ReadOptions, WriteBatch as EngineBatch};
use tessari_kv::{
    Error, Key, KeyRange, Keyspace, KvBackend, Result, ScanDirection, ScanRequest, Value,
    WriteBatch, WriteOp, delete_range_by_scanning,
};

mod group;
mod kv;
mod syncs;

use crate::AtRestKey;
use crate::encryption;
use crate::error::{BACKEND_NAME, from_engine, from_open, missing_region};
use crate::options::{Durability, StoreConfig, database_options, regions};

/// The engine property carrying the count of failed background jobs.
const BACKGROUND_ERRORS: &str = "rocksdb.background-errors";

/// A durable, ordered store.
///
/// Field order is the destruction order, and it is load-bearing: the database
/// must be torn down before anything its options refer to. Declaring the cache
/// after it is what makes that happen.
pub struct LsmBackend {
    database: DB,
    /// Held, never read. The options handed to the database refer to it, and an
    /// object an open database still points at must not be dropped first. The
    /// binding happens to reference-count this one, but that is the binding's
    /// implementation detail and not something the store should depend on.
    _cache: rocksdb::Cache,
    durability: Durability,
    path: PathBuf,
    /// Held for the whole of `apply`, which is what makes a precondition mean
    /// anything. See the module documentation.
    write_lock: Mutex<()>,
    /// What has landed in the WAL and how much of it a sync covers, so a
    /// round's sync is paid only when one is owed (`syncs`).
    syncs: syncs::WalSyncs,
}

/// Written by hand because the engine's cache handle carries no `Debug`, and
/// because what is worth printing about a store is where it is and what it
/// promises — not the engine state behind it.
impl std::fmt::Debug for LsmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LsmBackend")
            .field("path", &self.path)
            .field("durability", &self.durability.name())
            .finish_non_exhaustive()
    }
}

impl LsmBackend {
    /// Open the store at `path`, creating it when the directory holds none.
    ///
    /// Creation and opening are separated deliberately. A store that exists must
    /// already contain every region this build expects; a region that is absent
    /// is a refusal, not something to add in place, because a store with an
    /// unexpected region set was written by something with a different idea of
    /// what it contains.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unavailable`] when the directory is already open in
    /// another process, [`Error::Validation`] when an existing store is missing a
    /// region, and the mapped engine failure otherwise.
    pub fn open(path: impl AsRef<Path>, config: StoreConfig) -> Result<Self> {
        Self::open_with_key(path, config, None)
    }

    /// Open the store at `path`, encrypted under `key` when one is given
    /// (ADR-0108 D7).
    ///
    /// A new store given a key is created encrypted. An existing store opens
    /// only the way it was created: an encrypted one refuses to open without
    /// its key or under another, and a plain one refuses a key — each refusal
    /// saying which, before the engine reads a byte.
    ///
    /// # Errors
    ///
    /// As [`LsmBackend::open`], and [`Error::Validation`] for a key that does
    /// not match the store.
    pub fn open_with_key(
        path: impl AsRef<Path>,
        config: StoreConfig,
        key: Option<&AtRestKey>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let cache = rocksdb::Cache::new_lru_cache(config.block_cache_bytes);

        // `CURRENT` names the engine's live manifest, so it is there exactly
        // when a store is.
        encryption::admit(&path, key, path.join("CURRENT").exists())?;
        let environment = key.map(encryption::environment).transpose()?;
        let mut listing = Options::default();
        if let Some(environment) = &environment {
            listing.set_env(environment);
        }
        let existing = DB::list_cf(&listing, &path).ok();
        if let Some(found) = &existing {
            let missing: Vec<Keyspace> = Keyspace::ALL
                .iter()
                .copied()
                .filter(|keyspace| !found.iter().any(|name| name == keyspace.name()))
                .collect();
            if !missing.is_empty() {
                return Err(missing_region(&missing, &path));
            }
        }

        let create = existing.is_none();
        let mut database_options = database_options(&config, create);
        if let Some(environment) = &environment {
            database_options.set_env(environment);
        }
        let descriptors = regions(&cache)
            .into_iter()
            .map(|(name, options)| rocksdb::ColumnFamilyDescriptor::new(name, options));

        let database = DB::open_cf_descriptors(&database_options, &path, descriptors)
            .map_err(|error| from_open(&error, &path))?;

        Ok(Self {
            database,
            _cache: cache,
            durability: config.durability,
            path,
            write_lock: Mutex::new(()),
            syncs: syncs::WalSyncs::default(),
        })
    }

    /// What an acknowledged write is promised to survive.
    #[must_use]
    pub const fn durability(&self) -> Durability {
        self.durability
    }

    /// How many background jobs have failed.
    ///
    /// A non-zero count means a flush or compaction failed, which can leave the
    /// engine refusing writes. Nothing reports that on its own, so a health
    /// check has to ask — the alternative is learning about it from a user's
    /// failed write.
    ///
    /// This walks engine state, so it belongs on a health interval and not on a
    /// request path.
    ///
    /// # Errors
    ///
    /// Returns the mapped engine failure when the property cannot be read.
    pub fn background_errors(&self) -> Result<u64> {
        self.database
            .property_int_value(BACKGROUND_ERRORS)
            .map_err(|error| from_engine(&error))
            .map(Option::unwrap_or_default)
    }

    /// Compact every region, now, and wait for it.
    ///
    /// An operational tool rather than something the store needs: the engine
    /// compacts on its own schedule, and this exists for the two moments when
    /// waiting for that schedule is wrong — after a mass delete, when the space
    /// is wanted back before the next natural compaction reaches those levels;
    /// and in a test, where "the engine has compacted" has to be a fact rather
    /// than a hope.
    ///
    /// **It cannot change an answer.** A compaction rewrites how records are
    /// stored and never what they are, so every live record is still readable
    /// and every deleted one still absent afterwards — which is the property the
    /// readiness checklist asks to see asserted rather than assumed, because a
    /// compaction that dropped a live record would do it silently.
    ///
    /// **It triggers a flush, and that is safe here for a reason worth naming.**
    /// A manual compaction flushes the memtable first, and a per-region flush is
    /// exactly what `atomic_flush` was set to prevent — a crash between two
    /// regions' flushes could restore one past the other. Checked at the code
    /// path rather than assumed: RocksDB 11.8.1
    /// `db/db_impl/db_impl_compaction_flush.cc:1341` routes
    /// `CompactRangeInternal` to `AtomicFlushMemTables` when `atomic_flush` is
    /// set, so the flush a compaction triggers covers every region even though
    /// this call names one. KB `invariant-wal-protects-regions` carries the
    /// argument, and this method is why that entry had to be revisited.
    ///
    /// Expensive by construction: it rewrites every level of every region. Not
    /// something to put on a timer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownKeyspace`] when a region is missing, which would
    /// mean a store this build did not open.
    pub fn compact(&self) -> Result<()> {
        for keyspace in Keyspace::ALL.iter().copied() {
            let region = self.region(keyspace)?;
            // No bounds: every key of the region, which is what "forced" means.
            self.database
                .compact_range_cf(&region, None::<&[u8]>, None::<&[u8]>);
        }
        Ok(())
    }

    /// Every engine counter, as the engine formats them.
    ///
    /// `pub(crate)` and used by one test: the counters are how a claim about the
    /// block cache is checked without timing anything, and `enable_statistics()`
    /// is already set in `database_options`. It is not on the public surface
    /// because nothing outside this crate has asked for it, and a metrics
    /// surface is a decision rather than a side effect of needing one counter.
    #[cfg(test)]
    pub(crate) fn statistics(&self) -> Option<String> {
        self.database
            .property_value(rocksdb::properties::OPTIONS_STATISTICS)
            .ok()
            .flatten()
    }

    /// Read a range, keeping its blocks or not.
    ///
    /// The two trait methods differ in exactly this one argument, so they share
    /// a body: a sweep that answered differently from a scan would be a defect
    /// nothing in the caller could see.
    fn read_range(&self, request: &ScanRequest, caching: Caching) -> Result<Vec<(Key, Value)>> {
        if request.range.is_provably_empty() {
            return Ok(Vec::new());
        }
        let region = self.region(request.keyspace)?;
        let mode = match request.direction {
            ScanDirection::Forward => IteratorMode::Start,
            ScanDirection::Reverse => IteratorMode::End,
        };

        let mut collected = Vec::new();
        let limit = request.limit.unwrap_or(usize::MAX);
        let iterator =
            self.database
                .iterator_cf_opt(region, read_options(&request.range, caching), mode);
        for entry in iterator {
            if collected.len() >= limit {
                break;
            }
            let (key, value) = entry.map_err(|error| from_engine(&error))?;
            collected.push((Key::new(key.into_vec()), Value::new(value.into_vec())));
        }
        Ok(collected)
    }

    /// Flush everything buffered and hand the directory back.
    ///
    /// A clean close saves the work that recovery would otherwise redo.
    ///
    /// # Errors
    ///
    /// Returns the mapped engine failure when the flush does not complete.
    pub fn close(self) -> Result<()> {
        self.database
            .flush_wal(true)
            .map_err(|error| from_engine(&error))
    }

    fn region(&self, keyspace: Keyspace) -> Result<&ColumnFamily> {
        // Resolved per call rather than stored, because the engine binding ties
        // a region handle's lifetime to the database handle and a struct cannot
        // hold both. The lookup stays inside this one method so that no caller
        // ever holds a handle.
        self.database
            .cf_handle(keyspace.name())
            .ok_or_else(|| Error::UnknownKeyspace {
                keyspace: keyspace.name().to_owned(),
            })
    }

    /// Apply `batch` under `options`, subject to its preconditions — the one
    /// write both [`KvBackend::apply`] and [`KvBackend::apply_unsynced`] make.
    fn write(&self, batch: WriteBatch, synced: bool) -> Result<()> {
        let _writer = self.writer();
        let before = self.syncs.landed_so_far();

        // Every precondition is read here, under the lock, so nothing can move
        // between the check and the write below.
        for precondition in batch.preconditions() {
            let region = self.region(precondition.keyspace())?;
            let observed = self
                .database
                .get_cf(region, precondition.key().as_slice())
                .map_err(|error| from_engine(&error))?
                .map(Value::new);
            if !precondition.is_satisfied_by(observed.as_ref()) {
                return Err(Error::Conflict {
                    keyspace: precondition.keyspace().name().to_owned(),
                    key: precondition.key().clone(),
                });
            }
        }

        // Every region an operation names is resolved before any of them is
        // written, so an unknown one fails the batch instead of splitting it.
        let mut engine_batch = EngineBatch::default();
        for op in batch.ops() {
            let region = self.region(op.keyspace())?;
            match op {
                WriteOp::Put { key, value, .. } => {
                    engine_batch.put_cf(region, key.as_slice(), value.as_slice());
                }
                WriteOp::Delete { key, .. } => {
                    engine_batch.delete_cf(region, key.as_slice());
                }
            }
        }

        let mut options = rocksdb::WriteOptions::default();
        options.set_sync(synced);
        self.database
            .write_opt(engine_batch, &options)
            .map_err(|error| from_engine(&error))?;
        self.syncs.landed(before, synced);
        Ok(())
    }

    /// The write lock, taken as found even after a panic elsewhere held it.
    ///
    /// It guards no data: what it orders is a precondition check and one engine
    /// batch, and the batch lands whole or not at all. So a poisoned lock says
    /// nothing about the store, and refusing on it would leave a live node that
    /// can no longer write until it is restarted.
    fn writer(&self) -> MutexGuard<'_, ()> {
        self.write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Turn a range into the bounds the iterator walks between.
///
/// The engine takes a half-open span, so the two bound shapes it does not have
/// are expressed by moving to the neighbouring key. Appending a zero byte gives
/// the smallest key strictly greater than the original, which is what both an
/// exclusive lower bound and an inclusive upper bound need.
/// Whether a read's blocks are worth keeping.
///
/// Not a `bool`, because a `bool` at a call site says nothing about which way
/// round it is — and the two callers of [`read_options`] differ only here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caching {
    /// Ordinary reads, whose blocks the next read is likely to want.
    Fill,
    /// A one-shot walk of a whole table, whose blocks nothing will ask for
    /// again. Keeping them evicts the working set the store is serving.
    Skip,
}

fn read_options(range: &KeyRange, caching: Caching) -> ReadOptions {
    use std::ops::Bound;

    let mut options = ReadOptions::default();
    if caching == Caching::Skip {
        // The engine's own guidance for a one-shot read: a scan that touches
        // more blocks than the cache holds evicts the whole working set to hold
        // data nothing will read again.
        options.fill_cache(false);
    }
    match range.start() {
        Bound::Included(key) => options.set_iterate_lower_bound(key.as_slice().to_vec()),
        Bound::Excluded(key) => options.set_iterate_lower_bound(successor(key)),
        Bound::Unbounded => {}
    }
    match range.end() {
        Bound::Excluded(key) => options.set_iterate_upper_bound(key.as_slice().to_vec()),
        Bound::Included(key) => options.set_iterate_upper_bound(successor(key)),
        Bound::Unbounded => {}
    }
    options
}

/// The smallest key strictly greater than this one.
fn successor(key: &Key) -> Vec<u8> {
    let mut bytes = key.as_slice().to_vec();
    bytes.push(0x00);
    bytes
}

/// Move an already-open iterator to the first pair of one range.
///
/// The iterator carries no bounds of its own, because it serves many ranges in
/// turn. Both halves of a range are therefore applied here: the start decides
/// where to seek, and the end decides whether what was found belongs to this
/// range at all.
fn seek_first(
    iterator: &mut rocksdb::DBRawIterator<'_>,
    range: &KeyRange,
) -> Result<Option<(Key, Value)>> {
    use std::ops::Bound;

    if range.is_provably_empty() {
        return Ok(None);
    }
    match range.start() {
        Bound::Included(key) => iterator.seek(key.as_slice()),
        Bound::Excluded(key) => iterator.seek(successor(key)),
        Bound::Unbounded => iterator.seek_to_first(),
    }
    if !iterator.valid() {
        // Past the last key is an answer; a read failure is not, and only
        // asking distinguishes them.
        iterator.status().map_err(|error| from_engine(&error))?;
        return Ok(None);
    }
    let (Some(key), Some(value)) = (iterator.key(), iterator.value()) else {
        return Ok(None);
    };
    let within = match range.end() {
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Excluded(end) => key < end.as_slice(),
        Bound::Unbounded => true,
    };
    if !within {
        return Ok(None);
    }
    Ok(Some((Key::new(key.to_vec()), Value::new(value.to_vec()))))
}

#[cfg(test)]
mod tests;
