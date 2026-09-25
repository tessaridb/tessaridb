//! Removing record versions whose instant has passed.
//!
//! Every removal is an ordinary `DELETE` through a transaction: sequenced into
//! the log, carried to followers, seen on the change feed, and fenced by the
//! same leadership and lease every write is. So on a node that may not write
//! the range, the commit is refused and nothing diverges — the pass does not
//! need to know who leads.

use tessari_encoding::{ExpiryKey, StoreKey};
use tessari_kv::{ScanDirection, ScanRequest};

use crate::error::Result;
use crate::store::Store;
use crate::transaction::RecordAddress;

/// How many entries one pass takes into a single commit.
///
/// Bounded for the reason the series pass is: a store that has been down while
/// a million keys expired should not answer with one million-row commit.
const BATCH_ENTRIES: usize = 512;

/// What one pass removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Lapsed {
    /// Records removed because their instant had passed.
    pub records: usize,
    /// How many commits it took.
    pub batches: usize,
    /// Entries whose record no longer carries that instant.
    ///
    /// Zero by construction — the index is maintained in the records' own
    /// batch — and counted rather than silently skipped, because a non-zero
    /// figure here is the only place a broken index would ever show.
    pub stale: usize,
}

impl Store {
    /// Remove every record version whose expiry has passed, a batch at a time.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, a stored key or value cannot be
    /// decoded, or a commit is refused — which is what a node that does not lead
    /// the range meets, by design.
    pub fn remove_expired(&self) -> Result<Lapsed> {
        let mut lapsed = Lapsed::default();
        loop {
            let mut transaction = self.begin()?;
            let now = transaction.clock();
            let entries = self.backend().scan(&ScanRequest {
                keyspace: ExpiryKey::keyspace(),
                range: ExpiryKey::passed_by(now),
                direction: ScanDirection::Forward,
                limit: Some(BATCH_ENTRIES),
            })?;
            let mut removed = 0_usize;
            for (key, _) in &entries {
                let entry = ExpiryKey::decode(key.as_slice())?;
                let address =
                    RecordAddress::new(entry.namespace, entry.database, entry.table, entry.id);
                let current = transaction
                    .read_stamped_at(&address)?
                    .and_then(|stamped| stamped.expires());
                if current == Some(entry.at) {
                    transaction.delete(address);
                    removed = removed.saturating_add(1);
                } else {
                    lapsed.stale = lapsed.stale.saturating_add(1);
                }
            }
            if removed == 0 {
                return Ok(lapsed);
            }
            transaction.commit()?;
            lapsed.records = lapsed.records.saturating_add(removed);
            lapsed.batches = lapsed.batches.saturating_add(1);
        }
    }
}
