//! Table definitions already decoded, keyed by table and confirmed by the bytes
//! they were decoded from.
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
//! A remembered definition is handed back only for the very bytes it was decoded
//! from. The row is still read, through the reading transaction and at its
//! snapshot, exactly as before; only turning those bytes into a definition is
//! remembered. A definition that changed was written as different bytes and
//! misses, a transaction reading an older snapshot reads the older bytes and gets
//! the older definition, and nothing here has to be told that anything changed.
//! There is no invalidation because there is nothing that can go stale.
//!
//! The table is the key and the bytes the confirmation, rather than the bytes
//! the key, because a key is hashed on every read and a stored definition is
//! hundreds of bytes long. The price is one table holding one version: two
//! readers alternating between an old snapshot and a new one decode on each
//! turn, which is the window of a redefinition and nothing more.
//!
//! # Bound
//!
//! [`HELD`] definitions, plus at most one per statement missing at the same
//! moment: the size is read and the set cleared shard by shard, so two misses
//! racing past the bound both insert. Past it the whole set is dropped and
//! refilled by what is being read now, which is simpler than an eviction order
//! and costs one decode per table in use.

use std::sync::Arc;

use dashmap::DashMap;

use tessari_encoding::decode_payload;

use tessari_types::TableId;

use crate::catalog::TableDefinition;
use crate::error::Result;

/// How many decoded definitions a process keeps.
const HELD: usize = 1024;

/// Decoded table definitions, by table, each with the bytes it came from.
#[derive(Debug, Default)]
pub(crate) struct DecodedTables {
    /// A `DashMap`: every statement reads a decoded definition from
    /// every thread, and a sharded map lets those reads take different locks
    /// where one `RwLock` had every reader bounce the same counter.
    held: DashMap<TableId, Held>,
}

/// One table's definition and the stored bytes it was decoded from.
#[derive(Debug)]
struct Held {
    stored: Vec<u8>,
    definition: Arc<TableDefinition>,
}

impl DecodedTables {
    /// The definition these stored bytes hold, shared rather than copied: a
    /// statement asks for its table's definition several times, and every
    /// answer is the same immutable value.
    pub(crate) fn definition(&self, table: TableId, stored: &[u8]) -> Result<Arc<TableDefinition>> {
        // The guard is a shard lock, so it ends with this block, before the
        // insert below reaches the same map.
        if let Some(found) = self.held.get(&table)
            && found.stored == stored
        {
            return Ok(Arc::clone(&found.definition));
        }
        let definition = Arc::new(decode(stored)?);
        self.hold(table, stored, Arc::clone(&definition));
        Ok(definition)
    }

    /// The definition stored bytes hold, for a caller that has the bytes and not
    /// yet the table: decoded, and remembered under the table it names.
    pub(crate) fn decoded(&self, stored: &[u8]) -> Result<Arc<TableDefinition>> {
        let definition = Arc::new(decode(stored)?);
        self.hold(definition.id, stored, Arc::clone(&definition));
        Ok(definition)
    }

    fn hold(&self, table: TableId, stored: &[u8], definition: Arc<TableDefinition>) {
        if self.held.len() >= HELD {
            self.held.clear();
        }
        self.held.insert(
            table,
            Held {
                stored: stored.to_vec(),
                definition,
            },
        );
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.held.len()
    }
}

/// The definition stored bytes hold.
fn decode(stored: &[u8]) -> Result<TableDefinition> {
    TableDefinition::from_value(&decode_payload(stored)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use crate::now::Now;
    use std::sync::Arc;

    use tessari_kv::{KvBackend, MemoryBackend};
    use tessari_types::RecordId;

    use super::{DecodedTables, HELD};
    use tessari_types::TableId;

    /// The table a stored row defines.
    fn id_of(stored: &[u8]) -> TableId {
        super::decode(stored).unwrap().id
    }
    use crate::Store;
    use crate::catalog::{Catalog, TableDefinition, TableShape, system};

    /// A store holding `count` tables, and each table's stored definition row.
    fn stored_rows(count: u32) -> Vec<Vec<u8>> {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
            .now()
            .unwrap();
        let mut transaction = store.begin().now().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns").now().unwrap().id;
        let database = catalog.create_database(namespace, "db").now().unwrap().id;
        let ids: Vec<_> = (0..count)
            .map(|n| {
                catalog
                    .create_table(namespace, database, &format!("t{n}"), TableShape::default())
                    .now()
                    .unwrap()
                    .id
            })
            .collect();
        ids.iter()
            .map(|id| {
                let address = system::address(system::TABLES, RecordId::Int(i64::from(id.get())));
                transaction.get(&address).now().unwrap().unwrap()
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
            assert_eq!(
                *decoded.definition(direct.id, &stored).unwrap(),
                direct,
                "first read"
            );
            assert_eq!(
                *decoded.definition(direct.id, &stored).unwrap(),
                direct,
                "remembered read"
            );
        }
    }

    #[test]
    fn different_bytes_are_different_definitions() {
        let rows = stored_rows(2);
        let decoded = DecodedTables::default();
        let first = decoded.definition(id_of(&rows[0]), &rows[0]).unwrap();
        let second = decoded.definition(id_of(&rows[1]), &rows[1]).unwrap();
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
        let before = decoded.definition(id_of(&row), &row).unwrap();
        let mut changed = (*before).clone();
        changed.schemafull = !changed.schemafull;
        let rewritten = tessari_encoding::encode_payload(&changed.to_value());
        assert_eq!(
            *decoded.definition(before.id, rewritten.as_slice()).unwrap(),
            changed
        );
        assert_eq!(decoded.definition(before.id, &row).unwrap(), before);
    }

    #[test]
    fn a_full_set_is_emptied_before_it_grows_past_its_bound() {
        let row = stored_rows(1).remove(0);
        let decoded = DecodedTables::default();
        let definition = decoded.definition(id_of(&row), &row).unwrap();
        decoded.held.clear();
        for n in 0..HELD {
            decoded.held.insert(
                TableId::new(u32::try_from(n).unwrap() + 1_000_000),
                super::Held {
                    stored: Vec::new(),
                    definition: Arc::clone(&definition),
                },
            );
        }
        assert_eq!(decoded.len(), HELD);
        // A miss on a full set: the set is dropped and refilled from this read.
        assert_eq!(decoded.definition(id_of(&row), &row).unwrap(), definition);
        assert_eq!(decoded.len(), 1);
    }
}
