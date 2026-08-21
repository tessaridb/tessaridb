//! The store: the handle that owns the backend and the committed tail.
//!
//! One type owns the substrate handle for its lifetime, resolves the store's
//! on-disk format at open, and hands out transactions. Everything above it
//! speaks in records and sequences; nothing above it sees a key, a keyspace or
//! a batch.

use std::ops::Bound;
use std::sync::Arc;

use bgv_db_encoding::{
    AppliedPositionKey, FormatVersion, FormatVersionKey, LogKey, LogRecord, StoreKey, StoreValue,
};
use bgv_db_kv::{KeyRange, KvBackend, ScanDirection, ScanRequest, WriteBatch};
use bgv_db_types::Sequence;

use crate::error::{Error, Result};
use crate::snapshots::Registry;
use crate::transaction::Transaction;

/// A record store over a key-value backend.
///
/// Cloning a store shares one backend **and one snapshot registry**: two handles
/// to the same store are not two stores, and a floor computed from half the live
/// readers would reclaim versions the other half is still reading.
#[derive(Debug, Clone)]
pub struct Store {
    backend: Arc<dyn KvBackend>,
    snapshots: Arc<Registry>,
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
        let store = Self {
            backend,
            snapshots: Arc::new(Registry::default()),
        };
        match store.read_format_version()? {
            Some(found) => found.check_supported()?,
            None => store.write_initial_metadata()?,
        }
        Ok(store)
    }

    /// Begin a transaction at the current committed tail.
    ///
    /// # Errors
    ///
    /// Returns an error when the committed tail cannot be read or decoded.
    pub fn begin(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new(self, self.committed_tail()?))
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
    /// # Errors
    ///
    /// Returns [`Error::LogGap`] when the record is not the next one, and the
    /// mapped backend or decoding failure otherwise.
    pub fn apply_record(&self, at: Sequence, record: &LogRecord) -> Result<()> {
        let applied = self.committed_tail()?;
        if at.get() <= applied.get() {
            return Ok(());
        }
        let expected = Sequence::new(applied.get().saturating_add(1));
        if at != expected {
            return Err(Error::LogGap {
                expected,
                found: at,
            });
        }
        let batch = crate::index::maintain(self, record, crate::log::apply_batch(at, record))?;
        self.backend.apply(batch)?;
        Ok(())
    }

    /// The backend, for the transaction's read and commit paths.
    pub(crate) fn backend(&self) -> &Arc<dyn KvBackend> {
        &self.backend
    }

    fn read_format_version(&self) -> Result<Option<FormatVersion>> {
        let key = FormatVersionKey.encode();
        let stored = self.backend.get(FormatVersionKey::keyspace(), &key)?;
        match stored {
            Some(value) => Ok(Some(FormatVersion::decode(value.as_slice())?)),
            None => Ok(None),
        }
    }

    /// Write the metadata a fresh store needs, refusing if someone raced us.
    ///
    /// The `Absent` precondition is what makes two processes opening the same
    /// new store safe: exactly one of them writes the metadata.
    fn write_initial_metadata(&self) -> Result<()> {
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
        self.backend.apply(batch)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use bgv_db_kv::MemoryBackend;

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
            store.read_format_version().unwrap(),
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
            second.read_format_version().unwrap(),
            Some(FormatVersion::CURRENT)
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
