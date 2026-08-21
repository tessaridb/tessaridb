//! Keeping index entries in step with the records they describe.
//!
//! # Entries are derived, never logged
//!
//! A log record carries record mutations and nothing else. Index entries are
//! computed from those mutations when the record is applied, which is what makes
//! a replica's indexes match the leader's without anything being sent: apply is
//! a pure function of the log entry and the catalog, and the catalog is itself
//! in the log, so every replica computes the same entries at the same sequence.
//!
//! Carrying the entries in the log would work too, and would cost log size and
//! a second source of truth that a rebuild could disagree with.
//!
//! # Two rules that follow from the value system
//!
//! **A record missing an indexed field is not indexed at all.** `none` means the
//! field is not there, so there is no value to place. Indexing it as `none`
//! instead would make every record lacking the field collide in a unique index,
//! which is a constraint nobody asked for.
//!
//! **A record whose field holds `null` *is* indexed**, under `null`. It is a
//! value, and two records holding it collide in a unique index the same way two
//! records holding `7` do. That is the point of keeping absent and null apart.
//!
//! # An index is a candidate set, not an answer
//!
//! Index entries hold the **current** state: they carry no version, and an
//! update removes the entry for the value it replaced. A transaction reading at
//! an older snapshot therefore cannot trust an index scan on its own — it must
//! resolve each candidate record at its own snapshot, and it may miss a record
//! whose indexed value has since changed. Reading at the latest committed state
//! is exact.
//!
//! That is a real limitation and it is written here rather than discovered by
//! the first query that returns the wrong rows. Removing it means versioning the
//! entries and reclaiming old ones in the background, which is a larger piece of
//! work than this one and is not started.
//!
//! # What is not built here
//!
//! An index created on a table that already has rows indexes **none of them**.
//! Maintenance only sees mutations, so existing records need a backfill, and a
//! backfill needs a resumable watermark — the key kind is reserved (`0x37`) and
//! the work is not done. Until it is, an index is trustworthy only on a table it
//! was created on before the first write.

use std::collections::{BTreeMap, BTreeSet};

use bgv_db_encoding::{
    IndexAddress, IndexTarget, IndexValues, LogRecord, Mutation, NoPayload, RecordValue,
    SecondaryIndexKey, StoreKey, StoreValue, UniqueIndexKey, decode_payload,
};
use bgv_db_kv::WriteBatch;
use bgv_db_types::{RecordId, TableId, Value};

use crate::catalog::{Catalog, IndexDefinition};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::RecordAddress;

/// Add the index writes a log record implies to `batch`.
///
/// Reads the catalog and the records' current values as of the committed state,
/// which is the state this record is about to be applied on top of.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    let mut view = store.begin()?;
    let mut by_table: BTreeMap<TableId, Vec<IndexDefinition>> = BTreeMap::new();
    // Two records in ONE batch claiming one unique value would each find the key
    // absent and each write it, and the second would silently overwrite the
    // first. A precondition cannot catch that — both are satisfied — so the
    // claims are tracked here.
    let mut claimed: BTreeSet<Vec<u8>> = BTreeSet::new();

    for mutation in record.mutations() {
        let definitions = match by_table.get(&mutation.table) {
            Some(found) => found.clone(),
            None => {
                let found = Catalog::new(&mut view).indexes_on(mutation.table)?;
                by_table.insert(mutation.table, found.clone());
                found
            }
        };
        if definitions.is_empty() {
            continue;
        }

        let address = RecordAddress::new(
            mutation.namespace,
            mutation.database,
            mutation.table,
            mutation.id.clone(),
        );
        let previous = view.get(&address)?;

        for definition in &definitions {
            batch = apply_one(
                store,
                batch,
                definition,
                mutation,
                previous.as_deref(),
                &mut claimed,
            )?;
        }
    }
    Ok(batch)
}

fn apply_one(
    store: &Store,
    mut batch: WriteBatch,
    definition: &IndexDefinition,
    mutation: &Mutation,
    previous: Option<&[u8]>,
    claimed: &mut BTreeSet<Vec<u8>>,
) -> Result<WriteBatch> {
    let address = IndexAddress::new(
        definition.namespace,
        definition.database,
        definition.table,
        definition.id,
    );

    // The old entry goes first: a record whose indexed value changed must not
    // leave the entry that pointed at its former value behind, and an entry
    // nothing will ever reconcile is the failure mode secondary indexes are
    // known for.
    if let Some(bytes) = previous {
        if let Some(values) = project(definition, &decode_payload(bytes)?) {
            batch = remove(batch, definition, &address, &values, &mutation.id);
        }
    }

    if let RecordValue::Present(payload) = &mutation.value {
        if let Some(values) = project(definition, &decode_payload(payload)?) {
            batch = insert(
                store,
                batch,
                definition,
                &address,
                &values,
                &mutation.id,
                claimed,
            )?;
        }
    }
    Ok(batch)
}

fn insert(
    store: &Store,
    batch: WriteBatch,
    definition: &IndexDefinition,
    address: &IndexAddress,
    values: &IndexValues,
    id: &RecordId,
    claimed: &mut BTreeSet<Vec<u8>>,
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
    if !claimed.insert(key.as_slice().to_vec()) {
        return Err(violation());
    }

    // The precondition is what makes this safe against a concurrent writer: the
    // check below reads committed state, and without it a second transaction
    // could claim the value between the read and the apply.
    let batch = match store.backend().get(keyspace, &key)? {
        Some(existing) => {
            if IndexTarget::decode(existing.as_slice())?.id != *id {
                return Err(violation());
            }
            batch.expect_value(keyspace, key.clone(), existing)
        }
        None => batch.expect_absent(keyspace, key.clone()),
    };
    Ok(batch.put(keyspace, key, IndexTarget::new(id.clone()).encode()))
}

fn remove(
    batch: WriteBatch,
    definition: &IndexDefinition,
    address: &IndexAddress,
    values: &IndexValues,
    id: &RecordId,
) -> WriteBatch {
    if definition.unique {
        let key = UniqueIndexKey::new(*address, values.clone());
        batch.delete(UniqueIndexKey::keyspace(), key.encode())
    } else {
        let key = SecondaryIndexKey::new(*address, values.clone(), id.clone());
        batch.delete(SecondaryIndexKey::keyspace(), key.encode())
    }
}

/// The indexed fields of one record, or `None` when the record is not indexed.
///
/// A record that is not an object has no fields to project, and a record missing
/// one of the indexed fields has no value to place — both mean "not in this
/// index" rather than "indexed under nothing".
fn project(definition: &IndexDefinition, value: &Value) -> Option<IndexValues> {
    let Value::Object(fields) = value else {
        return None;
    };
    let mut projected = Vec::with_capacity(definition.fields.len());
    for name in &definition.fields {
        match fields.get(name) {
            Some(Value::None) | None => return None,
            Some(found) => projected.push(found.clone()),
        }
    }
    Some(IndexValues::of(&projected))
}
