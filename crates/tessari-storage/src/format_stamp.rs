//! A finalize, applied: the record travels in the log and every replica raises
//! its own node-local stamp in the batch that applies it (ADR-0118 D3).
//!
//! Derived on both paths — the leader's commit and a follower's apply — for the
//! reason every other derived write is: a stamp only the leader moved would leave
//! each follower opening as the older format it no longer holds.

use tessari_encoding::{FormatVersion, FormatVersionKey, LogRecord, StoreKey, StoreValue};
use tessari_kv::WriteBatch;

use crate::catalog::finalized_in;
use crate::error::Result;
use crate::store::Store;

/// Raise the stamp to the format `record` finalizes the store to, if it does.
///
/// Never lowers it: a finalize to the format the store already holds, or below
/// it, changes nothing.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    for mutation in record.mutations() {
        let Some(finalized) = finalized_in(mutation)? else {
            continue;
        };
        if store.held_format()? < finalized {
            batch = batch.put(
                FormatVersionKey::keyspace(),
                FormatVersionKey.encode(),
                finalized.encode(),
            );
        }
    }
    Ok(batch)
}

/// The stamp a store opened by this build should hold, given the format its
/// catalog says it was finalized to.
///
/// A follower upgraded after the finalize was applied by its older build, which
/// kept the record and never moved the stamp; this is where it catches up. Only
/// to a format this build can write — above that the stamp is left, and the
/// store is opened as the format it actually holds.
pub(crate) fn caught_up(
    held: FormatVersion,
    finalized: Option<FormatVersion>,
) -> Option<FormatVersion> {
    finalized.filter(|finalized| *finalized > held && *finalized <= FormatVersion::CURRENT)
}
