//! The records a containment index offers for a document asked for (ADR-0116).

use std::collections::{BTreeMap, BTreeSet};

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{ContainmentKey, IndexAddress, IndexValues, RecordValue, StoreKey};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::RecordId;

use super::address::resuming_after;
use super::{RecordAddress, StoredRecord, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

impl Transaction<'_> {
    /// The records holding **every** pair asked for, with their stored bytes,
    /// and this transaction's own writes to the table folded in.
    ///
    /// A **candidate set and never an answer**: a path forgets where in an array
    /// a leaf was, so `{ a: [[1], 2] }` holds the pairs of `{ a: [[1, 2]] }` and
    /// contains nothing of it. The caller re-tests every record against the
    /// whole condition, as above every other index read in this store.
    ///
    /// The pairs are walked one at a time and the set only ever narrows, so a
    /// read stops reading entries the moment no record holds them all. A record
    /// this transaction wrote has no entries yet and is offered unconditionally;
    /// one it deleted is withdrawn — the fold every index read here makes.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or an entry cannot be decoded.
    pub fn records_containing(
        &self,
        index: &IndexDefinition,
        pairs: &[IndexValues],
    ) -> Result<Vec<StoredRecord>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let keyspace = ContainmentKey::keyspace();
        let mut held: Option<BTreeSet<RecordId>> = None;
        for pair in pairs {
            let prefix = ContainmentKey::pair_prefix(&address, pair);
            let range = KeyRange::prefix(&prefix);
            let mut holding = BTreeSet::new();
            let mut from: Option<Vec<u8>> = None;
            loop {
                let request = ScanRequest {
                    keyspace,
                    range: match &from {
                        Some(after) => KeyRange::from_bounds(
                            std::ops::Bound::Included(Key::from(after.clone())),
                            range.end().clone(),
                        ),
                        None => range.clone(),
                    },
                    direction: ScanDirection::Forward,
                    limit: Some(RANGE_SCAN_BATCH_ENTRIES),
                };
                let batch = self.store.backend().scan(&request)?;
                for (key, _) in &batch {
                    let id = ContainmentKey::decode(key.as_slice())?.id;
                    // Narrowing: only a record every earlier pair offered is kept.
                    if held.as_ref().is_none_or(|kept| kept.contains(&id)) {
                        holding.insert(id);
                    }
                }
                let Some((last, _)) = batch
                    .last()
                    .filter(|_| batch.len() >= RANGE_SCAN_BATCH_ENTRIES)
                else {
                    break;
                };
                from = Some(resuming_after(last.as_slice().to_vec()));
            }
            let empty = holding.is_empty();
            held = Some(holding);
            if empty {
                break;
            }
        }

        let addresses: Vec<RecordAddress> = held
            .unwrap_or_default()
            .into_iter()
            .map(|id| RecordAddress::new(index.namespace, index.database, index.table, id))
            .collect();
        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        for (address, payload) in addresses.iter().zip(self.get_each(&addresses)?) {
            if let Some(payload) = payload {
                found.insert(address.id.clone(), payload);
            }
        }
        for (address, value) in &self.writes {
            if address.namespace != index.namespace
                || address.database != index.database
                || address.table != index.table
            {
                continue;
            }
            match value {
                RecordValue::Present(payload) => {
                    found.insert(address.id.clone(), payload.clone());
                }
                RecordValue::Tombstone => {
                    found.remove(&address.id);
                }
            }
        }
        Ok(found.into_iter().collect())
    }
}
