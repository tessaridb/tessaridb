//! Writing and removing one record's index entries, cells included.

use super::{covering_of, project};
use crate::catalog::IndexDefinition;
use crate::error::{Error, Result};
use crate::store::Store;
use tessari_encoding::{
    ContainmentKey, IndexAddress, IndexTarget, IndexValues, Mutation, NoPayload, RecordValue,
    SecondaryIndexKey, SpatialExtent, SpatialIndexKey, StoreKey, StoreValue, UniqueIndexKey,
    decode_payload,
};
use tessari_geo::{Bounds, Cell};
use tessari_kv::{Key, Keyspace, WriteBatch, WriteOp};
use tessari_types::{RecordId, Value};

pub(crate) async fn insert(
    store: &Store,
    batch: WriteBatch,
    definition: &IndexDefinition,
    address: &IndexAddress,
    values: &IndexValues,
    id: &RecordId,
    claims: &mut Claims,
) -> Result<WriteBatch> {
    if !definition.unique {
        let key = SecondaryIndexKey::new(*address, values.clone(), id.clone());
        return Ok(batch.put(
            SecondaryIndexKey::keyspace(),
            key.encode(),
            NoPayload.encode(),
        ));
    }

    let keyspace = UniqueIndexKey::keyspace();
    let key = UniqueIndexKey::new(*address, values.clone()).encode();
    let violation = || Error::UniqueViolation {
        index: definition.name.clone(),
        id: id.clone(),
    };
    if claims
        .claimed
        .insert_async(key.as_slice().to_vec())
        .await
        .is_err()
    {
        return Err(violation());
    }

    // The precondition is what makes this safe against a concurrent writer: the
    // check below reads committed state, and without it a second transaction
    // could claim the value between the read and the apply.
    //
    // Reading committed state alone could not see **this** transaction's own
    // removal, so a batch that deleted the record holding a value and then wrote
    // another record with it was refused by the index it was maintaining —
    // naming, as the offender, a record the same batch was about to delete. The
    // batch is asked instead. Ordering settles the rest: the delete was queued
    // first and this put is queued last, so the entry the batch leaves behind is
    // this one.
    //
    // The batch only knows the removals queued so far, and a log record's
    // mutations are applied in address order rather than statement order: a
    // holder whose id sorts after this record is released later in the same
    // batch. [`Claims::released`] holds every value the record gives up,
    // computed before any mutation is applied, so the order does not decide.
    //
    // The concurrency guarantee is untouched, because the precondition below is
    // still the committed entry: a second transaction that claims the value
    // first still wins and this batch still fails.
    let batch = match store.backend().get(keyspace, &key).await? {
        Some(existing) => {
            if !(releases(&batch, keyspace, &key)
                || claims.released.contains_async(key.as_slice()).await)
                && IndexTarget::decode(existing.as_slice())?.id != *id
            {
                return Err(violation());
            }
            batch.expect_value(keyspace, key.clone(), existing)
        }
        None => batch.expect_absent(keyspace, key.clone()),
    };
    Ok(batch.put(keyspace, key, IndexTarget::new(id.clone()).encode()))
}

/// The unique values one log record claims and gives up, across all its mutations.
///
/// Membership is all either set is asked, so neither keeps an order.
#[derive(Debug, Default)]
pub(crate) struct Claims {
    /// Unique index keys this batch has already written.
    ///
    /// Two records in ONE batch claiming one unique value would each find the
    /// key absent and each write it, and the second would silently overwrite the
    /// first. A precondition cannot catch that — both are satisfied.
    claimed: scc::HashSet<Vec<u8>>,
    /// Unique index keys some mutation of this batch gives up: held by the
    /// record before and not by what the mutation leaves.
    released: scc::HashSet<Vec<u8>>,
}

impl Claims {
    /// Note the unique values `mutation` releases, given the record it replaces.
    ///
    /// A value the record keeps is not released, so a record rewritten in the
    /// same batch still refuses another record that wants its value.
    pub(crate) async fn release(
        &self,
        definitions: &[IndexDefinition],
        mutation: &Mutation,
        previous: Option<&[u8]>,
    ) -> Result<()> {
        let Some(bytes) = previous else {
            return Ok(());
        };
        if !definitions.iter().any(|definition| definition.unique) {
            return Ok(());
        }
        let before = decode_payload(bytes)?;
        let after = match mutation.value.value() {
            RecordValue::Present(payload) => Some(decode_payload(payload)?),
            RecordValue::Tombstone => None,
        };
        for definition in definitions.iter().filter(|definition| definition.unique) {
            let address = IndexAddress::new(
                definition.namespace,
                definition.database,
                definition.table,
                definition.id,
            );
            let kept = after
                .as_ref()
                .map(|value| project(definition, value))
                .unwrap_or_default();
            for values in project(definition, &before) {
                if !kept.contains(&values) {
                    let key = UniqueIndexKey::new(address, values).encode();
                    // A key already noted is the same release twice, not a conflict.
                    let _ = self.released.insert_async(key.as_slice().to_vec()).await;
                }
            }
        }
        Ok(())
    }
}

/// Whether this batch already deletes an index entry.
///
/// Asked per key, never "does the batch delete anything": a transaction that
/// releases one unique value has not released every one of them.
pub(crate) fn releases(batch: &WriteBatch, keyspace: Keyspace, key: &Key) -> bool {
    batch.ops().iter().any(|op| match op {
        WriteOp::Delete {
            keyspace: space,
            key: dropped,
        } => *space == keyspace && dropped == key,
        WriteOp::Put { .. } => false,
    })
}

pub(crate) async fn remove(
    batch: WriteBatch,
    definition: &IndexDefinition,
    address: &IndexAddress,
    values: &IndexValues,
    id: &RecordId,
    claims: &Claims,
) -> WriteBatch {
    if definition.unique {
        let key = UniqueIndexKey::new(*address, values.clone()).encode();
        // Another record of this batch took the value already, earlier in
        // address order; deleting the key now would delete ITS entry and leave
        // the value held by nobody the index knows of.
        if claims.claimed.contains_async(key.as_slice()).await {
            return batch;
        }
        batch.delete(UniqueIndexKey::keyspace(), key)
    } else {
        let key = SecondaryIndexKey::new(*address, values.clone(), id.clone());
        batch.delete(SecondaryIndexKey::keyspace(), key.encode())
    }
}

/// The (path, leaf) pairs one record's indexed document holds, each as the two
/// index values of one containment entry (ADR-0116 D4).
///
/// The one enumeration both sides of a write use, so a record that kept a leaf
/// writes back the key it already had and one that lost it leaves none behind.
pub(crate) fn containment_pairs(definition: &IndexDefinition, record: &Value) -> Vec<IndexValues> {
    definition
        .fields
        .first()
        .and_then(|field| field.resolve(record))
        .map(|held| {
            tessari_types::containment::held_pairs(held)
                .iter()
                .map(|pair| IndexValues::of(pair))
                .collect()
        })
        .unwrap_or_default()
}

/// Every pair of one record's document, written into `batch`.
pub(crate) fn contain(
    mut batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    record: &Value,
    definition: &IndexDefinition,
) -> WriteBatch {
    for values in containment_pairs(definition, record) {
        batch = batch.put(
            ContainmentKey::keyspace(),
            ContainmentKey::new(*address, values, id.clone()).encode(),
            NoPayload.encode(),
        );
    }
    batch
}

/// Every pair of one record's document, deleted from `batch`.
pub(crate) fn uncontain(
    mut batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    record: &Value,
    definition: &IndexDefinition,
) -> WriteBatch {
    for values in containment_pairs(definition, record) {
        batch = batch.delete(
            ContainmentKey::keyspace(),
            ContainmentKey::new(*address, values, id.clone()).encode(),
        );
    }
    batch
}

/// Every cell of one record's geometry, written into `batch`.
///
/// The record's bounding box travels in each entry's value, and it is computed
/// **here** — inside the batch that carries the record's own mutation, by the
/// writer, once. Nothing else may compute it: a box maintained by a background
/// job or recomputed by a reader can lag the geometry it describes, and a stale
/// box excludes rows that should have matched with nothing raised anywhere. That
/// is the one failure direction a spatial filter must not have, and keeping the
/// computation on this path is the whole of the defence.
pub(crate) fn place(
    batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    value: &Value,
    definition: &IndexDefinition,
) -> WriteBatch {
    let Some((bounds, cells)) = covering_of(definition, value) else {
        return batch;
    };
    place_cells(batch, address, id, bounds, &cells)
}

/// The write half of [`place`], for a caller that already holds the covering.
///
/// Split out so a build can write the entries and measure them from **one**
/// computed covering rather than two. Computing it twice would cost a second
/// pass and, worse, would let the entries and the figure describing them be
/// derived from separately-computed cells.
pub(crate) fn place_cells(
    mut batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    bounds: Bounds,
    cells: &[Cell],
) -> WriteBatch {
    let extent = SpatialExtent::new(bounds).encode();
    for cell in cells {
        batch = batch.put(
            SpatialIndexKey::keyspace(),
            SpatialIndexKey::new(*address, *cell, id.clone()).encode(),
            extent.clone(),
        );
    }
    batch
}

/// Every cell of one record's geometry, deleted from `batch`.
pub(crate) fn displace(
    mut batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    value: &Value,
    definition: &IndexDefinition,
) -> WriteBatch {
    let Some((_, cells)) = covering_of(definition, value) else {
        return batch;
    };
    for cell in cells {
        batch = batch.delete(
            SpatialIndexKey::keyspace(),
            SpatialIndexKey::new(*address, cell, id.clone()).encode(),
        );
    }
    batch
}
