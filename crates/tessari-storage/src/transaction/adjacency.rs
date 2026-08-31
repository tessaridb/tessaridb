//! Reading a node's neighbours.
//!
//! One range read over the node's own prefix, which is the whole reason the
//! adjacency layout exists. The alternative it replaces is an index probe
//! followed by a random read of every edge record, and at depth three over a
//! fanned-out node that is thousands of random reads.
//!
//! **Adjacency entries carry no version**, exactly as index entries do not: they
//! describe the committed tail. A read of the past therefore cannot be served
//! from them, and the caller refuses such a traversal rather than answering it
//! from today's edges over yesterday's records — the same guard the index-backed
//! traversal already keeps.

use tessari_encoding::{AdjacencyKey, Direction, EdgeProperties, StoreKey, StoreValue};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, TableId, Value};

use super::Transaction;
use crate::catalog::EdgeKindDefinition;
use crate::error::Result;

/// One neighbour, and what the edge reaching it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbour {
    /// The table the neighbour lives in.
    pub table: TableId,
    /// The neighbour itself.
    pub id: RecordId,
    /// The edge's own properties, decoded.
    pub properties: Value,
}

impl Transaction<'_> {
    /// The neighbours one node reaches under one edge kind, in one direction.
    ///
    /// A single range read: the entries are contiguous because the key puts the
    /// node above the edge kind and the kind above the direction, so the answer
    /// is a prefix rather than a filter over something wider.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored entry cannot be decoded.
    pub fn neighbours(
        &self,
        kind: &EdgeKindDefinition,
        node_table: TableId,
        node: &RecordId,
        direction: Direction,
    ) -> Result<Vec<Neighbour>> {
        let keyspace = AdjacencyKey::keyspace();
        // The kind carries its own tenancy, graph and id, so they are read from
        // it rather than restated at the call site: seven loose components there
        // is one transposition away from reading a different graph's adjacency,
        // and nothing about the answer would look wrong.
        let prefix = AdjacencyKey::hop_prefix(
            kind.namespace,
            kind.database,
            kind.graph,
            node_table,
            node,
            kind.id,
            direction,
        );
        let request = ScanRequest {
            keyspace,
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut found = Vec::new();
        for (key, value) in self.store.backend().scan(&request)? {
            let entry = AdjacencyKey::decode(key.as_slice())?;
            let properties = EdgeProperties::decode(value.as_slice())?;
            found.push(Neighbour {
                table: entry.neighbour_table,
                id: entry.neighbour,
                properties: if properties.is_empty() {
                    Value::Object(std::collections::BTreeMap::new())
                } else {
                    tessari_encoding::decode_payload(properties.as_slice())?
                },
            });
        }
        Ok(found)
    }
}
