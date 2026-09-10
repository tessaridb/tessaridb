//! Removing what a series table has stopped answering with.
//!
//! The floor hides a record; this removes it. They are deliberately two acts —
//! that separation is the engine's safety property, because correctness comes
//! from the read and a pass that lags, is throttled or has never run costs
//! storage rather than an answer.
//!
//! # It writes through the ordinary path, and that is the whole of its
//! replication story
//!
//! A removal here is a `DELETE` like any other: sequenced into the commit log,
//! carried over the protocol, and visible on the change feed as
//! [`crate::ChangeKind::Removed`]. Nothing about it is a second route into the
//! store. So a follower does not need to run this pass, and the question of
//! **who** runs it is a cluster question rather than a storage one.
//!
//! One consequence belongs to whoever reads a feed: a subscriber sees these
//! removals, and it sees them **after** the records stopped being visible to a
//! reader. A consumer mirroring a series table therefore holds rows the source
//! no longer shows, for as long as this pass lags.
//!
//! # What it does not do
//!
//! It does not free space. A removal is a tombstone at a new version, so the
//! bytes come back through the store's ordinary version reclamation — the same
//! sentence the specification already writes about the retention statement.
//! [`Store::reclaim_table`] is that pass, and its own documentation records why
//! it has no schedule: when to run it is an operational decision that wants a
//! measurement rather than a constant chosen while writing the code. The same
//! is true here, which is why this module offers a callable pass and not a
//! thread.

use std::collections::BTreeMap;

use tessari_encoding::{RecordKey, RecordValue, StoreKey, StoreValue};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use crate::error::Result;
use crate::store::Store;
use crate::transaction::RecordAddress;

/// How many records one pass removes in a single commit.
///
/// Bounded rather than "everything below the floor in one transaction", because
/// a retention run that matched a very large table would otherwise be a very
/// large commit — which is the shape the specification warns about for the
/// statement form and the reason a synchronous range delete is the wrong
/// mechanism at scale. A pass that is interrupted between batches has removed a
/// prefix of what it was going to remove, and running it again finishes the job:
/// the work is idempotent because the floor is a function of the clock and the
/// records, not of how far a previous run got.
const BATCH_RECORDS: usize = 512;

/// What one pass over one table removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Expired {
    /// How many records were removed.
    pub records: usize,
    /// How many commits it took.
    pub batches: usize,
}

impl Store {
    /// Remove the records one series table has stopped answering with.
    ///
    /// Answers `Expired::default()` for a table that is not a series, so a
    /// caller sweeping a database does not have to ask what each table is.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, a stored key or value cannot be
    /// decoded, or a commit is refused.
    pub fn expire_series(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Expired> {
        let mut removed = Expired::default();
        loop {
            let doomed = self.below_the_floor(namespace, database, table)?;
            if doomed.is_empty() {
                return Ok(removed);
            }
            let mut transaction = self.begin()?;
            for id in &doomed {
                transaction.delete(RecordAddress::new(namespace, database, table, id.clone()));
            }
            transaction.commit()?;
            removed.records = removed.records.saturating_add(doomed.len());
            removed.batches = removed.batches.saturating_add(1);
        }
    }

    /// The next batch of records past the floor that are still present.
    ///
    /// Reads the keyspace directly rather than through a table read, and it has
    /// to: an ordinary read of a series table **cannot see** what is past the
    /// floor, which is the whole point of the floor. So this is the one place in
    /// the store that deliberately looks below it, and it looks at keys rather
    /// than at records because it does not need what they say.
    fn below_the_floor(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<RecordId>> {
        let transaction = self.begin()?;
        let Some(floor) = transaction.series_floor(namespace, table)? else {
            return Ok(Vec::new());
        };
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::between(
                RecordKey::table_prefix(namespace, database, table).into(),
                RecordKey::versions_prefix(namespace, database, table, &floor).into(),
            ),
            direction: ScanDirection::Forward,
            limit: None,
        };

        // Versions of one record are adjacent and sort newest-first, so the
        // first entry for an identity is the one that says whether the record is
        // still there. A record whose newest version is already a tombstone is
        // skipped — without that the pass would write a tombstone under an
        // identity that is itself below the floor, find it again on the next
        // run, and grow the table it was asked to shrink.
        let mut newest: BTreeMap<RecordId, bool> = BTreeMap::new();
        for (key, value) in self.backend().scan(&request)? {
            let decoded = RecordKey::decode(key.as_slice())?;
            if newest.contains_key(&decoded.id) {
                continue;
            }
            let present = matches!(
                RecordValue::decode(value.as_slice())?,
                RecordValue::Present(_)
            );
            newest.insert(decoded.id, present);
            if newest.len() >= BATCH_RECORDS {
                break;
            }
        }
        Ok(newest
            .into_iter()
            .filter_map(|(id, present)| present.then_some(id))
            .collect())
    }
}
