//! Nodes the cluster removed and never admits again (ADR-0108 D9).
//!
//! Dropping a peer's row stops counting it, but the node keeps its identity and
//! its certificate, both still valid. Without a record of the removal, a node
//! dropped for cause could greet again and be bound to the next row an operator
//! opened. So a drop of a row that named a node writes the node here, and from
//! then on that identity is refused at the door, never bound, and never named by
//! a new row. A machine coming back is wiped and starts under a new identity —
//! the practice of every consensus system that keeps removed members out.
//!
//! A row and not a file, for the revocation list's reason: a removal is a
//! cluster-wide fact, carried to every node whatever it follows.

use std::collections::BTreeMap;

use tessari_encoding::{NODE_ID_LEN, decode_payload, encode_payload};
use tessari_types::{RecordId, Value};

use super::{Catalog, system};
use crate::error::{Error, Result};

const ENTITY: &str = "tombstoned node";
const FIELD_NODE: &str = "node";

impl Catalog<'_, '_> {
    /// Never admit `node` again.
    pub fn tombstone_node(&mut self, node: [u8; NODE_ID_LEN]) {
        let row = Value::Object(BTreeMap::from([(FIELD_NODE.to_owned(), Value::Uuid(node))]));
        self.transaction.put(
            system::address(system::TOMBSTONED_NODES, RecordId::Uuid(node)),
            encode_payload(&row).into_bytes(),
        );
    }

    /// Whether `node` was removed.
    ///
    /// # Errors
    ///
    /// A backend failure.
    pub fn is_tombstoned(&self, node: &[u8; NODE_ID_LEN]) -> Result<bool> {
        Ok(self
            .transaction
            .get(&system::address(
                system::TOMBSTONED_NODES,
                RecordId::Uuid(*node),
            ))?
            .is_some())
    }

    /// Every removed node, in order.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a tombstone.
    pub fn tombstoned_nodes(&self) -> Result<Vec<[u8; NODE_ID_LEN]>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::TOMBSTONED_NODES,
        )? {
            let row = decode_payload(&payload)?;
            let node = match &row {
                Value::Object(fields) => fields.get(FIELD_NODE),
                _ => None,
            };
            match node {
                Some(Value::Uuid(node)) => found.push(*node),
                other => {
                    return Err(Error::CatalogMalformed {
                        entity: ENTITY,
                        field: FIELD_NODE,
                        found: other.map_or_else(|| row.type_name(), Value::type_name),
                    });
                }
            }
        }
        Ok(found)
    }
}
