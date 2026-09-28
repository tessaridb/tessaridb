//! Table definitions already decoded, keyed by the bytes they were decoded from.
//!
//! # Why this exists
//!
//! Every statement and every commit asks the catalog for the definitions of the
//! tables it touches — resolving the name, checking the schema, the space limit,
//! the topic rules — and each answer was the stored row decoded into a value and
//! then into a [`TableDefinition`]. Profiled on 2026-09-27 (G040 SG3), that
//! decoding and the reads around it were half of a point read's time and most of
//! the work a commit does while it holds the write gate.
//!
//! # Why it cannot serve a stale definition
//!
//! The key is the stored row itself. The row is still read, through the reading
//! transaction and at its snapshot, exactly as before; only turning those bytes
//! into a definition is remembered. A definition that changed was written as
//! different bytes and misses, a transaction reading an older snapshot reads the
//! older bytes and gets the older definition, and nothing here has to be told
//! that anything changed. There is no invalidation because there is nothing a
//! key can go stale against.
//!
//! # Bound
//!
//! At most [`HELD`] definitions. Past it the whole set is dropped and refilled
//! by what is being read now, which is simpler than an eviction order and costs
//! one decode per table in use.

use std::collections::HashMap;
use std::sync::RwLock;

use tessari_encoding::decode_payload;

use crate::catalog::TableDefinition;
use crate::error::Result;

/// How many decoded definitions a process keeps.
const HELD: usize = 1024;

/// Decoded table definitions, keyed by their stored bytes.
#[derive(Debug, Default)]
pub(crate) struct DecodedTables {
    /// A `RwLock`: every statement reads a decoded definition, and a write
    /// happens only the first time a definition's bytes are seen (G040 M1).
    held: RwLock<HashMap<Vec<u8>, TableDefinition>>,
}

impl DecodedTables {
    /// The definition these stored bytes hold.
    ///
    /// A poisoned lock is read past rather than refused: every change under it
    /// is one map operation, and the answer is decoded directly either way.
    pub(crate) fn definition(&self, stored: &[u8]) -> Result<TableDefinition> {
        if let Ok(held) = self.held.read()
            && let Some(found) = held.get(stored)
        {
            return Ok(found.clone());
        }
        let decoded = TableDefinition::from_value(&decode_payload(stored)?)?;
        if let Ok(mut held) = self.held.write() {
            if held.len() >= HELD {
                held.clear();
            }
            held.insert(stored.to_vec(), decoded.clone());
        }
        Ok(decoded)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.held.read().map_or(0, |held| held.len())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use tessari_kv::{KvBackend, MemoryBackend};
    use tessari_types::RecordId;

    use super::{DecodedTables, HELD};
    use crate::Store;
    use crate::catalog::{Catalog, TableDefinition, TableShape, system};

    /// A store holding `count` tables, and each table's stored definition row.
    fn stored_rows(count: u32) -> Vec<Vec<u8>> {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns").unwrap().id;
        let database = catalog.create_database(namespace, "db").unwrap().id;
        let ids: Vec<_> = (0..count)
            .map(|n| {
                catalog
                    .create_table(namespace, database, &format!("t{n}"), TableShape::default())
                    .unwrap()
                    .id
            })
            .collect();
        ids.iter()
            .map(|id| {
                let address = system::address(system::TABLES, RecordId::Int(i64::from(id.get())));
                transaction.get(&address).unwrap().unwrap()
            })
            .collect()
    }

    #[test]
    fn a_remembered_definition_is_the_one_its_bytes_decode_to() {
        let decoded = DecodedTables::default();
        for stored in stored_rows(3) {
            let direct =
                TableDefinition::from_value(&tessari_encoding::decode_payload(&stored).unwrap())
                    .unwrap();
            assert_eq!(decoded.definition(&stored).unwrap(), direct, "first read");
            assert_eq!(
                decoded.definition(&stored).unwrap(),
                direct,
                "remembered read"
            );
        }
    }

    #[test]
    fn different_bytes_are_different_definitions() {
        let rows = stored_rows(2);
        let decoded = DecodedTables::default();
        let first = decoded.definition(&rows[0]).unwrap();
        let second = decoded.definition(&rows[1]).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(decoded.len(), 2);
    }

    #[test]
    fn a_table_redefined_is_read_as_redefined() {
        // One table, two stored versions of its definition. A set keyed by
        // anything coarser than the bytes — the table's id — would answer the
        // second read with the first definition.
        let row = stored_rows(1).remove(0);
        let decoded = DecodedTables::default();
        let before = decoded.definition(&row).unwrap();
        let mut changed = before.clone();
        changed.schemafull = !changed.schemafull;
        let rewritten = tessari_encoding::encode_payload(&changed.to_value());
        assert_eq!(decoded.definition(rewritten.as_slice()).unwrap(), changed);
        assert_eq!(decoded.definition(&row).unwrap(), before);
    }

    #[test]
    fn a_full_set_is_emptied_before_it_grows_past_its_bound() {
        let row = stored_rows(1).remove(0);
        let decoded = DecodedTables::default();
        let definition = decoded.definition(&row).unwrap();
        {
            let mut held = decoded.held.write().unwrap();
            held.clear();
            for n in 0..HELD {
                held.insert(n.to_le_bytes().to_vec(), definition.clone());
            }
        }
        assert_eq!(decoded.len(), HELD);
        // A miss on a full set: the set is dropped and refilled from this read.
        assert_eq!(decoded.definition(&row).unwrap(), definition);
        assert_eq!(decoded.len(), 1);
    }
}
