//! Transactions at snapshot isolation.
//!
//! A snapshot is one record version — this store's own number, not the log's
//! position (Q-614). Every read in a transaction seeks to that
//! sequence and takes the newest version at or before it, so the transaction
//! sees one consistent point in the store's history however long it runs.
//!
//! # What this level guarantees, and what it does not
//!
//! Guaranteed: reads are consistent as of the snapshot; a transaction sees its
//! own writes; and on a write-write race the first committer wins while the
//! loser writes nothing at all.
//!
//! **Not** guaranteed, and this is the level's defining limitation:
//!
//! - **Write skew.** Two transactions may each read a set, each find an
//!   invariant satisfied, each write a *different* key, and both commit —
//!   leaving the invariant violated with no conflict raised anywhere. Conflict
//!   detection is over what a transaction *wrote*, not over what it *read*.
//!   A caller that needs such an invariant materialises it into a key that both
//!   transactions write, which turns the skew into an ordinary detected
//!   conflict.
//! - **Phantoms.** Detection is per record, so a predicate re-evaluated later
//!   may match records that did not exist at snapshot time.
//!
//! Both are demonstrated by the test suite rather than described only here.

mod address;
mod adjacency;
mod commit;
mod index;
mod lifecycle;
mod ordered;
mod retention;
mod scan;
mod search;
mod spatial;

pub use address::{RecordAddress, StoredRecord};
pub use adjacency::Neighbour;
pub use search::Expansion;
pub use spatial::{Nearby, Region};

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use tessari_encoding::RecordValue;
use tessari_types::{RecordId, Sequence, TableId};

use crate::store::Store;

/// A unit of work at a fixed snapshot.
///
/// Dropping one releases its snapshot. That is deliberately not left to
/// [`Transaction::commit`] and [`Transaction::rollback`]: the retention floor is
/// bounded by the oldest live snapshot, so a transaction that is simply let go
/// of without either call would freeze reclamation for the life of the process —
/// silently, as space that never comes back.
#[derive(Debug)]
pub struct Transaction<'a> {
    store: &'a Store,
    snapshot: Sequence,
    writes: BTreeMap<RecordAddress, RecordValue>,
    /// The instant this transaction judges a series table's floor against.
    ///
    /// Read once, on the first question that needs it, for the reason the
    /// snapshot is fixed once: one read answers at one floor, or a walk long
    /// enough to cross the boundary refuses in its second batch what it returned
    /// in its first.
    reading_at: Cell<Option<u64>>,
    /// The floor per series table, derived once each.
    floors: RefCell<BTreeMap<TableId, Option<RecordId>>>,
}

impl Transaction<'_> {
    /// The millisecond this transaction judges retention against.
    fn reading_at(&self) -> u64 {
        if let Some(held) = self.reading_at.get() {
            return held;
        }
        // A clock before the epoch answers zero, which puts every floor at the
        // beginning of time and hides nothing. The alternative — refusing the
        // read — would turn a misconfigured host clock into an outage over data
        // that is present and correct.
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        self.reading_at.set(Some(millis));
        millis
    }
}
