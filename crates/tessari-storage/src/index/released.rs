//! A unique value held by a record that has expired is released, by deleting
//! that record in the commit that claims the value (ADR-0122 A7, Q-949).
//!
//! # Why a delete and not a looser check
//!
//! The unique entry names its holder, and a reader no longer sees a holder whose
//! instant has passed — but the entry, the holder's other index entries and its
//! expiry entry are all still there until the removal pass. Letting the new
//! record simply take the entry would leave two owners of one key in waiting:
//! when the pass later removed the expired holder it would take the key with it,
//! and the new record's entry would be gone with nothing in an error state.
//!
//! Deleting the holder in the same commit is the act the removal pass would have
//! done, done now: one ordinary tombstone in the log, ahead of the write that
//! claims the value, so every derived structure is maintained from it on the
//! held version (KB `failure-mode-derived-from-visible`) and a follower applies
//! the very same pair rather than judging an instant against its own clock.

use std::collections::BTreeSet;

use tessari_encoding::{
    IndexAddress, IndexTarget, LogRecord, Mutation, RecordValue, StampedValue, StoreKey,
    StoreValue, UniqueIndexKey, decode_payload,
};
use tessari_types::ShardId;

use super::project;
use crate::catalog::Catalog;
use crate::error::Result;
use crate::store::Store;
use crate::transaction::RecordAddress;

/// `record` with a deletion of every expired record holding a unique value one
/// of its writes claims, placed ahead of the write; `None` when there is none.
///
/// # Errors
///
/// A backend, catalog or decoding error.
pub(crate) fn release_expired_holders(
    store: &Store,
    record: &LogRecord,
    now: u64,
    node: [u8; tessari_encoding::NODE_ID_LEN],
    shard_of: impl Fn(&RecordAddress) -> Option<ShardId>,
) -> Result<Option<LogRecord>> {
    // Nothing in this store has ever expired, so nothing can hold a value it is
    // no longer answered for.
    if !store.expiring().seen() {
        return Ok(None);
    }
    let mut view = store.begin_local()?;
    let mut written: BTreeSet<RecordAddress> = record
        .mutations()
        .iter()
        .map(|mutation| {
            RecordAddress::new(
                mutation.namespace,
                mutation.database,
                mutation.table,
                mutation.id.clone(),
            )
        })
        .collect();
    let mut released = Vec::new();
    for mutation in record.mutations() {
        let RecordValue::Present(payload) = mutation.value.value() else {
            continue;
        };
        let unique: Vec<_> = Catalog::new(&mut view)
            .indexes_on(mutation.table)?
            .into_iter()
            .filter(|definition| {
                definition.unique
                    && definition.namespace == mutation.namespace
                    && definition.database == mutation.database
            })
            .collect();
        if unique.is_empty() {
            continue;
        }
        let value = decode_payload(payload)?;
        for definition in &unique {
            let address = IndexAddress::new(
                definition.namespace,
                definition.database,
                definition.table,
                definition.id,
            );
            for values in project(definition, &value) {
                let key = UniqueIndexKey::new(address, values).encode();
                let Some(entry) = store.backend().get(UniqueIndexKey::keyspace(), &key)? else {
                    continue;
                };
                let holder = IndexTarget::decode(entry.as_slice())?.id;
                if holder == mutation.id {
                    continue;
                }
                let held_at = RecordAddress::new(
                    mutation.namespace,
                    mutation.database,
                    mutation.table,
                    holder.clone(),
                );
                if written.contains(&held_at) {
                    continue;
                }
                let Some((_, held)) = view.read_newest_stamped(&held_at)? else {
                    continue;
                };
                if held.value().is_tombstone() || !held.is_expired_at(now) {
                    continue;
                }
                let mut stamp = held.stamp().clone();
                stamp.advance(node);
                released.push(Mutation {
                    namespace: mutation.namespace,
                    database: mutation.database,
                    table: mutation.table,
                    id: holder,
                    shard: shard_of(&held_at),
                    value: StampedValue::stamped(stamp, RecordValue::Tombstone),
                });
                written.insert(held_at);
            }
        }
    }
    if released.is_empty() {
        return Ok(None);
    }
    released.extend(record.mutations().iter().cloned());
    let mut carried = LogRecord::at(record.epoch(), released);
    if let Some(order) = record.order() {
        carried.set_order(order);
    }
    Ok(Some(carried))
}
