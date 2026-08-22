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
use std::sync::Mutex;

use bgv_db_kv::{
    Error, Key, KeyRange, Keyspace, KvBackend, Result, ScanDirection, ScanRequest, Value,
    WriteBatch, WriteOp,
};
use rocksdb::{ColumnFamily, DB, IteratorMode, Options, ReadOptions, WriteBatch as EngineBatch};

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
        let path = path.as_ref().to_path_buf();
        let cache = rocksdb::Cache::new_lru_cache(config.block_cache_bytes);

        let existing = DB::list_cf(&Options::default(), &path).ok();
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
        let database_options = database_options(&config, create);
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
}

impl KvBackend for LsmBackend {
    fn background_errors(&self) -> bgv_db_kv::Result<u64> {
        Self::background_errors(self)
    }

    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        let region = self.region(keyspace)?;
        let found = self
            .database
            .get_cf(region, key.as_slice())
            .map_err(|error| from_engine(&error))?;
        Ok(found.map(Value::new))
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
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
        let iterator = self
            .database
            .iterator_cf_opt(region, read_options(&request.range), mode);
        for entry in iterator {
            if collected.len() >= limit {
                break;
            }
            let (key, value) = entry.map_err(|error| from_engine(&error))?;
            collected.push((Key::new(key.into_vec()), Value::new(value.into_vec())));
        }
        Ok(collected)
    }

    /// One iterator, seeked many times, instead of one iterator per range.
    ///
    /// [`Self::scan`] creates an engine iterator per call, and creating one is
    /// not free: it pins the engine's view of the store for as long as it lives.
    /// Resolving the records an index range names issues one bounded read per
    /// record, so answering them one at a time creates one iterator per record —
    /// a per-item cost that grows with how much the store holds rather than with
    /// how large the answer is. Here the iterator is created once and moved.
    ///
    /// Each hit is checked against **its own** range's end before it is
    /// accepted. A seek positions at the first key at or after the target and
    /// knows nothing about where the caller wanted to stop, so without that
    /// check a range with nothing in it would answer with the next range's first
    /// key: a real pair, decodable, and the wrong record's.
    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        if ranges.is_empty() {
            return Ok(Vec::new());
        }
        let region = self.region(keyspace)?;
        let mut options = ReadOptions::default();
        // No prefix extractor is configured today, so a seek is already a
        // total-order seek. Saying so is what keeps this call correct if one is
        // ever configured: with an extractor and without this, a seek is allowed
        // to stop at the end of the target's prefix and report absence for a key
        // that exists.
        options.set_total_order_seek(true);
        let mut iterator = self.database.raw_iterator_cf_opt(region, options);

        let mut found = Vec::with_capacity(ranges.len());
        for range in ranges {
            found.push(seek_first(&mut iterator, range)?);
        }
        Ok(found)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        let _writer = self.write_lock.lock().map_err(|_| Error::Backend {
            backend: BACKEND_NAME,
            reason: "the write lock was poisoned by a panic in another thread".to_owned(),
            source: None,
        })?;

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

        self.database
            .write_opt(engine_batch, &self.durability.write_options())
            .map_err(|error| from_engine(&error))
    }
}

/// Turn a range into the bounds the iterator walks between.
///
/// The engine takes a half-open span, so the two bound shapes it does not have
/// are expressed by moving to the neighbouring key. Appending a zero byte gives
/// the smallest key strictly greater than the original, which is what both an
/// exclusive lower bound and an inclusive upper bound need.
fn read_options(range: &KeyRange) -> ReadOptions {
    use std::ops::Bound;

    let mut options = ReadOptions::default();
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
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_successor_is_greater_than_the_key_and_below_anything_after_it() {
        let key = Key::from_slice(b"ab");
        let next = successor(&key);
        assert!(next.as_slice() > key.as_slice());
        assert!(next.as_slice() < b"ab\x01".as_slice());
        assert!(next.as_slice() < b"ac".as_slice());
    }

    #[test]
    fn the_successor_of_the_empty_key_is_the_first_key_after_it() {
        assert_eq!(successor(&Key::from_slice(b"")), vec![0x00]);
    }
}
