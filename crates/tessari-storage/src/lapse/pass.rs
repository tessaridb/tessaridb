//! Removing record versions whose instant has passed.
//!
//! Every removal is an ordinary `DELETE` through a transaction: sequenced into
//! the log, carried to followers, seen on the change feed, and fenced by the
//! same leadership and lease every write is. So on a node that may not write a
//! range, the commit is refused and nothing diverges.
//!
//! # One commit per table, and a range led elsewhere is passed over
//!
//! The entries are ordered by instant across the whole store, so one batch of
//! them can name ranges with different leaders. Offered to one commit they are
//! refused as a whole — `SpansLeaderships` — and a node leading some of them
//! would remove none, every pass, while the node leading the rest did the same
//! (ADR-0122 A9, Q-950). So entries are committed a table at a time, a table led
//! elsewhere is left to its leader, and a split table whose shards have
//! different leaders falls back to one record per commit. The walk then moves on
//! past what it could not remove instead of stopping at it.

use std::collections::BTreeMap;
use std::ops::Bound;

use tessari_encoding::{ExpiryKey, StoreKey};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, TableId};

use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, resuming_after};

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
    /// Entries left because another node leads their range — removed there.
    pub elsewhere: usize,
}

impl Store {
    /// Remove every record version whose expiry has passed and whose range this
    /// node may write, a table at a time.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, a stored key or value cannot be
    /// decoded, or a commit is refused for a reason other than the range being
    /// led elsewhere.
    pub fn remove_expired(&self) -> Result<Lapsed> {
        let mut lapsed = Lapsed::default();
        let now = self.begin()?.clock();
        let whole = ExpiryKey::passed_by(now);
        let mut from: Option<Vec<u8>> = None;
        loop {
            let range = match &from {
                None => whole.clone(),
                Some(last) => KeyRange::from_bounds(
                    Bound::Included(Key::from(resuming_after(last.clone()))),
                    whole.end().clone(),
                ),
            };
            let page = self.backend().scan(&ScanRequest {
                keyspace: ExpiryKey::keyspace(),
                range,
                direction: ScanDirection::Forward,
                limit: Some(BATCH_ENTRIES),
            })?;
            let Some((last, _)) = page.last() else {
                return Ok(lapsed);
            };
            from = Some(last.as_slice().to_vec());
            let mut by_table: BTreeMap<(NamespaceId, DatabaseId, TableId), Vec<ExpiryKey>> =
                BTreeMap::new();
            for (key, _) in &page {
                let entry = ExpiryKey::decode(key.as_slice())?;
                by_table
                    .entry((entry.namespace, entry.database, entry.table))
                    .or_default()
                    .push(entry);
            }
            for entries in by_table.values() {
                self.remove_table_entries(entries, &mut lapsed)?;
            }
            if page.len() < BATCH_ENTRIES {
                return Ok(lapsed);
            }
        }
    }

    /// One table's passed entries, in one commit when its ranges share a
    /// leader and one record at a time when they do not.
    fn remove_table_entries(&self, entries: &[ExpiryKey], lapsed: &mut Lapsed) -> Result<()> {
        match self.remove_these(entries) {
            Ok(done) => {
                done.add_to(lapsed);
                Ok(())
            }
            Err(Error::SpansLeaderships { .. }) => {
                for entry in entries {
                    match self.remove_these(std::slice::from_ref(entry)) {
                        Ok(done) => done.add_to(lapsed),
                        Err(why) if led_elsewhere(&why) => {
                            lapsed.elsewhere = lapsed.elsewhere.saturating_add(1);
                        }
                        Err(why) => return Err(why),
                    }
                }
                Ok(())
            }
            Err(why) if led_elsewhere(&why) => {
                lapsed.elsewhere = lapsed.elsewhere.saturating_add(entries.len());
                Ok(())
            }
            Err(why) => Err(why),
        }
    }

    /// Delete the records these entries name whose version still carries the
    /// instant, in one commit.
    fn remove_these(&self, entries: &[ExpiryKey]) -> Result<Lapsed> {
        let mut transaction = self.begin()?;
        let mut done = Lapsed::default();
        for entry in entries {
            let address = RecordAddress::new(
                entry.namespace,
                entry.database,
                entry.table,
                entry.id.clone(),
            );
            let current = transaction
                .read_stamped_at(&address)?
                .and_then(|stamped| stamped.expires());
            if current == Some(entry.at) {
                transaction.delete(address);
                done.records = done.records.saturating_add(1);
            } else {
                done.stale = done.stale.saturating_add(1);
            }
        }
        if done.records > 0 {
            transaction.commit()?;
            done.batches = 1;
        }
        Ok(done)
    }
}

impl Lapsed {
    fn add_to(self, total: &mut Self) {
        total.records = total.records.saturating_add(self.records);
        total.batches = total.batches.saturating_add(self.batches);
        total.stale = total.stale.saturating_add(self.stale);
        total.elsewhere = total.elsewhere.saturating_add(self.elsewhere);
    }
}

/// Whether a commit was refused because another node writes the range — the
/// design rather than a fault, and the leader's pass removes the record.
fn led_elsewhere(why: &Error) -> bool {
    matches!(
        why,
        Error::LeaseSpent { .. }
            | Error::NoLeadershipYet
            | Error::WriteIsElsewhere { .. }
            | Error::SpansLeaderships { .. }
    )
}
