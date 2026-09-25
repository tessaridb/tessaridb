//! Adjacency maintenance — a node's neighbours, derived from the edge's own
//! mutation.
//!
//! # Why this is derived here and not written by the caller
//!
//! It sits beside [`crate::index`] and runs from the same place in the commit,
//! because it needs the same guarantee for the same reason. The write batch is
//! built from the [`LogRecord`], and a replica reaches its state by replaying
//! that record — so anything a caller merely *added to the batch* on the leader
//! would never exist on a follower. The symptom would be a follower whose walks
//! find nothing while the leader answers correctly, with nothing anywhere in an
//! error state.
//!
//! Deriving from the mutation instead gives adjacency everything the record path
//! already has: atomicity with the records at both ends, MVCC versioning,
//! replication through the shared apply path, and the schema check.
//!
//! # The rule this module exists to keep
//!
//! **Both entries, in the batch that carries the edge.** An entry written outside
//! that batch is an orphan nothing will ever reconcile: either the edge is gone
//! and a walk still reaches through it, or the edge is there and no walk finds
//! it. Neither raises anything, and neither is discoverable except by a sweep.
//!
//! Deletion is the harder half, because removing an edge needs the **old**
//! endpoints to know which two keys to delete — so a tombstone reads the previous
//! version first, exactly as an index update does. Endpoints are immutable (an
//! edge is identified by them), so a property change rewrites the values under
//! keys that do not move.
//!
//! `DROP EDGE` needs no path of its own for the same reason: it deletes the
//! kind's records, those become tombstones in this record, and the entries go
//! with them in the batch that carries the deletion. A parallel range-delete
//! would be a second way to remove an entry, and two ways to remove one thing is
//! how one of them ends up forgotten.

use std::collections::BTreeMap;

use tessari_encoding::{
    AdjacencyKey, Direction, EdgeProperties, LogRecord, RecordValue, StoreKey, StoreValue,
    decode_payload, encode_payload,
};
use tessari_kv::WriteBatch;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

use crate::catalog::{Catalog, EDGE_IN, EDGE_OUT, EdgeKindDefinition};
use crate::error::Result;
use crate::store::Store;
use crate::transaction::RecordAddress;

/// Write the adjacency implied by everything one commit changed.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    let mut view = store.begin()?;
    // One catalog read per tenancy rather than per mutation. An edge kind is
    // found by its companion table, so the lookup is keyed by that table id.
    let mut by_tenancy: BTreeMap<(NamespaceId, DatabaseId), BTreeMap<TableId, EdgeKindDefinition>> =
        BTreeMap::new();

    for mutation in record.mutations() {
        let tenancy = (mutation.namespace, mutation.database);
        let kinds = match by_tenancy.get(&tenancy) {
            Some(found) => found,
            None => {
                let found = Catalog::new(&mut view)
                    .edge_kinds_in(tenancy.0, tenancy.1)?
                    .into_iter()
                    .map(|kind| (kind.edges, kind))
                    .collect();
                by_tenancy.entry(tenancy).or_insert(found)
            }
        };
        let Some(kind) = kinds.get(&mutation.table).cloned() else {
            continue;
        };

        // The previous version is read whether the mutation writes or deletes:
        // an edge that is being replaced still has old entries, and they are
        // under the old endpoints rather than the new ones.
        let address = RecordAddress::new(
            mutation.namespace,
            mutation.database,
            mutation.table,
            mutation.id.clone(),
        );
        if let Some(previous) = view.get_held(&address)?
            && let Some((from, to, _)) = endpoints(&decode_payload(&previous)?)
        {
            batch = write_pair(batch, &kind, &from, &to, None);
        }

        if let RecordValue::Present(payload) = mutation.value.value()
            && let Some((from, to, properties)) = endpoints(&decode_payload(payload)?)
        {
            batch = write_pair(batch, &kind, &from, &to, Some(&properties));
        }
    }
    Ok(batch)
}

/// The two endpoints an edge record carries, and everything else it holds.
///
/// Answers `None` rather than failing for a record without them: a companion
/// table holds nothing else today, and a decode error here would fail an
/// unrelated commit rather than the write that was actually wrong.
fn endpoints(value: &Value) -> Option<(Endpoint, Endpoint, Value)> {
    let Value::Object(fields) = value else {
        return None;
    };
    let from = endpoint(fields.get(EDGE_OUT)?)?;
    let to = endpoint(fields.get(EDGE_IN)?)?;
    let mut rest = fields.clone();
    rest.remove(EDGE_OUT);
    rest.remove(EDGE_IN);
    Some((from, to, Value::Object(rest)))
}

fn endpoint(value: &Value) -> Option<Endpoint> {
    match value {
        Value::Record(reference) => Some(Endpoint {
            table: reference.table,
            id: reference.id.clone(),
        }),
        _ => None,
    }
}

/// One end of an edge.
struct Endpoint {
    table: TableId,
    id: RecordId,
}

/// Put or delete both entries for one edge.
///
/// One function for both directions, because the two are a pair: a caller that
/// wrote them separately could write one and forget the other, and the store
/// would be wrong in a way only a sweep could find.
fn write_pair(
    batch: WriteBatch,
    kind: &EdgeKindDefinition,
    from: &Endpoint,
    to: &Endpoint,
    properties: Option<&Value>,
) -> WriteBatch {
    let out = AdjacencyKey::new(
        kind.namespace,
        kind.database,
        kind.graph,
        from.table,
        from.id.clone(),
        kind.id,
        Direction::Out,
        to.table,
        to.id.clone(),
    );
    let entries = [out.mirror(), out];
    let keyspace = AdjacencyKey::keyspace();
    entries.into_iter().fold(batch, |batch, entry| {
        let key = entry.encode();
        match properties {
            Some(value) => {
                let payload = EdgeProperties::new(encode_payload(value).into_bytes());
                batch.put(keyspace, key, payload.encode())
            }
            None => batch.delete(keyspace, key),
        }
    })
}
