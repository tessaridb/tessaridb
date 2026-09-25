//! Record versions that stop being answered at a stated instant (G035).
//!
//! # Two acts, as with a series table's floor
//!
//! A reader hides an expired version — `Transaction::get` and every walk judge
//! each version against the transaction's clock — and that alone makes every
//! answer right. Removing the bytes is a separate act, [`pass`], and a pass that
//! lags, is throttled or never runs costs storage and never an answer.
//!
//! # The expiry index, and why it is written with the records
//!
//! The pass must find what has expired without walking every table, so each
//! expiring version has an entry keyed by its instant (`ExpiryKey`). The entry is
//! derived from the log record and written **in the same batch** as the version,
//! on the commit path and on a follower's apply path alike — an entry written
//! anywhere else is an entry that can be left behind, and a promoted follower
//! would otherwise hold expiring records it has no way to find.
//!
//! The previous version's instant is read so its entry can go. That read is
//! skipped while the store has never held an expiring version at all
//! ([`Expiring`]), so a store that does not use the feature pays nothing for it
//! on the write path.

mod pass;

use std::sync::atomic::{AtomicBool, Ordering};

use tessari_encoding::{ExpiryKey, ExpiryMark, KeyKind, LogRecord, StoreKey, StoreValue};
use tessari_kv::{KeyRange, KvBackend, ScanDirection, ScanRequest, WriteBatch};

use crate::error::Result;
use crate::store::Store;
use crate::transaction::RecordAddress;

pub use pass::Lapsed;

/// Whether this store has ever held an expiring version.
///
/// Set once and never cleared: clearing it would need the index to be empty,
/// which is a scan, and a flag that is wrongly `true` costs one read per write
/// while one wrongly `false` leaves index entries behind.
#[derive(Debug, Default)]
pub(crate) struct Expiring(AtomicBool);

impl Expiring {
    /// Read the flag off the store: set when the expiry index holds any entry.
    pub(crate) fn load(backend: &dyn KvBackend) -> Result<Self> {
        let first = backend.scan(&ScanRequest {
            keyspace: KeyKind::ExpiryIndex.keyspace(),
            range: KeyRange::prefix(&[KeyKind::ExpiryIndex.tag()]),
            direction: ScanDirection::Forward,
            limit: Some(1),
        })?;
        Ok(Self(AtomicBool::new(!first.is_empty())))
    }

    fn seen(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn note(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Add the expiry-index writes a log record implies to `batch`.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    let view = store.begin()?;
    for mutation in record.mutations() {
        let next = mutation.value.expires();
        if next.is_some() {
            store.expiring().note();
        } else if !store.expiring().seen() {
            // Nothing in this store has ever expired, so no previous version
            // has an entry to take away.
            continue;
        }
        let address = RecordAddress::new(
            mutation.namespace,
            mutation.database,
            mutation.table,
            mutation.id.clone(),
        );
        let previous = view
            .read_stamped_at(&address)?
            .and_then(|stamped| stamped.expires());
        if previous == next {
            continue;
        }
        let entry = |at| ExpiryKey {
            at,
            namespace: mutation.namespace,
            database: mutation.database,
            table: mutation.table,
            id: mutation.id.clone(),
        };
        if let Some(at) = previous {
            batch = batch.delete(ExpiryKey::keyspace(), entry(at).encode());
        }
        if let Some(at) = next {
            batch = batch.put(
                ExpiryKey::keyspace(),
                entry(at).encode(),
                ExpiryMark.encode(),
            );
        }
    }
    Ok(batch)
}
