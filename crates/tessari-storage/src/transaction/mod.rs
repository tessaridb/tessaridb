//! Transactions at snapshot isolation.
//!
//! A snapshot is one sequence number. Every read in a transaction seeks to that
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
mod commit;
mod index;
mod lifecycle;
mod ordered;
mod scan;
mod search;
mod spatial;

pub use address::{RecordAddress, StoredRecord};
pub use spatial::{Nearby, Region};

use std::collections::BTreeMap;

use tessari_encoding::RecordValue;
use tessari_types::Sequence;

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
}
