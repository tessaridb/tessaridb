//! How many records a table holds, counted where they are written.
//!
//! # Why the planner had nothing to read
//!
//! The one per-table number the catalog kept is
//! [`RECORD_SEQUENCES`](crate::catalog::system::RECORD_SEQUENCES), and it
//! answers a different question. It is an identity allocator: it never
//! decreases when a record is deleted, and it is never touched when the caller
//! supplies its own id. A churned table would read far too large under it and a
//! table written with explicit ids would read zero. A planner that trusted it
//! would choose an access path silently and wrongly.
//!
//! # Why it is derived here rather than written by the caller
//!
//! It sits beside [`crate::index`] and [`crate::adjacency`] and runs from the
//! same places for the same reason. A replica reaches its state by replaying
//! the log record, so a number the leader merely added to its own batch would
//! never exist on a follower — and a follower whose planner reads a different
//! count answers the same query by a different path.
//!
//! # What this count is not
//!
//! It decides which access path a read takes. It never decides which records
//! that read returns. So a count that is stale, missing or wrong costs speed
//! and cannot cost correctness — which is why it is maintained in the write
//! batch rather than proven by a scan, why the arithmetic saturates rather than
//! refusing a commit, and why a table nothing has written reads as **absent**
//! rather than as zero. Absent means "no estimate", and a planner told nothing
//! behaves exactly as it did before this module existed.

use std::collections::BTreeMap;

use tessari_encoding::{
    LogRecord, RecordKey, RecordValue, StoreKey, StoreValue, decode_payload, encode_payload,
};
use tessari_kv::WriteBatch;
use tessari_types::{RecordId, Sequence, TableId};

use crate::catalog::{definition, system};
use crate::error::Result;
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};

/// Write the record counts implied by everything one commit changed.
///
/// # Errors
///
/// Returns an error when the store cannot be read or a held count cannot be
/// decoded.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
    version: Sequence,
) -> Result<WriteBatch> {
    let view = store.begin()?;
    let mut deltas: BTreeMap<TableId, i64> = BTreeMap::new();

    for mutation in record.mutations() {
        // Every catalog record lives in the system tenancy, and so do these
        // counters. Counting those would count the catalog — and would make
        // this module's own writes change the numbers it is writing.
        if mutation.namespace == system::SYSTEM_NAMESPACE {
            continue;
        }
        let address = RecordAddress::new(
            mutation.namespace,
            mutation.database,
            mutation.table,
            mutation.id.clone(),
        );
        // The previous version is read rather than assumed, because a write
        // over an existing record is a replacement and not an arrival, and a
        // delete of something already gone is not a departure. Both are zero,
        // and only the stored state tells them from the other two cases.
        let delta = match (view.get(&address)?.is_some(), &mutation.value) {
            (false, RecordValue::Present(_)) => 1_i64,
            (true, RecordValue::Tombstone) => -1_i64,
            _ => continue,
        };
        let running = deltas.entry(mutation.table).or_insert(0_i64);
        *running = running.saturating_add(delta);
    }

    for (table, delta) in deltas {
        if delta == 0 {
            continue;
        }
        batch = write_count(&view, batch, table, delta, version)?;
    }
    Ok(batch)
}

/// The count a table will hold once this commit lands.
fn write_count(
    view: &Transaction<'_>,
    batch: WriteBatch,
    table: TableId,
    delta: i64,
    version: Sequence,
) -> Result<WriteBatch> {
    let id = RecordId::Int(i64::from(table.get()));
    let address = system::address(system::RECORD_COUNTS, id.clone());
    let held = match view.get(&address)? {
        Some(bytes) => definition::count_of(&decode_payload(&bytes)?, "record count", "held")?,
        None => 0_u64,
    };
    // Saturating, and deliberately. A count that drifted below zero would be a
    // defect in this module, but refusing the commit would take a correct write
    // down with it to protect a number no answer depends on.
    let next = if delta >= 0 {
        held.saturating_add(delta.unsigned_abs())
    } else {
        held.saturating_sub(delta.unsigned_abs())
    };
    let key = RecordKey::new(
        system::SYSTEM_NAMESPACE,
        system::SYSTEM_DATABASE,
        system::RECORD_COUNTS,
        id,
        version,
    );
    let value = RecordValue::Present(encode_payload(&definition::count(next)?).into_bytes());
    Ok(batch.put(RecordKey::keyspace(), key.encode(), value.encode()))
}
