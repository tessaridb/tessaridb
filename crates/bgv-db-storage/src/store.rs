//! The store: the handle that owns the backend and the committed tail.
//!
//! One type owns the substrate handle for its lifetime, resolves the store's
//! on-disk format at open, and hands out transactions. Everything above it
//! speaks in records and sequences; nothing above it sees a key, a keyspace or
//! a batch.

use std::sync::Arc;

use bgv_db_encoding::{AppliedPositionKey, FormatVersion, FormatVersionKey, StoreKey, StoreValue};
use bgv_db_kv::{KvBackend, WriteBatch};
use bgv_db_types::Sequence;

use crate::error::Result;
use crate::transaction::Transaction;

/// A record store over a key-value backend.
#[derive(Debug, Clone)]
pub struct Store {
    backend: Arc<dyn KvBackend>,
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
        let store = Self { backend };
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
