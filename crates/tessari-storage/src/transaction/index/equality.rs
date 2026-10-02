//! A complete equality on a secondary index, walked in record order and stopped.
//!
//! # Why this one shape can stop when a range cannot
//!
//! A range's entries are ordered by value first, so the lowest identity among
//! its candidates is not known until every entry has been named — which is why
//! [`Transaction::walk_records_in_range`] collects the entry walk before it
//! hands anything over (ADR-0055). Under one **complete** value every entry
//! shares the whole value prefix, and what follows it is the record's identity,
//! encoded so that byte order is identity order. The entries are therefore
//! already the answer's order, and a walk that stops after ten has handed over
//! exactly the ten a scan would have reached first.
//!
//! # What it asks of the backend
//!
//! Entries a batch at a time and the records they name resolved together, both
//! from a small first batch that doubles: a caller that wants ten pays for a few
//! dozen rows, and one that wants every match reaches the full batch after a
//! handful of rounds and from there costs what the collecting walk did.
//!
//! # This transaction's own writes
//!
//! A record written here has no entry yet and one changed here may still have
//! the entry for its old value, so every pending record of the table is a
//! candidate and its pending payload is the one handed over; a pending delete
//! hands over nothing. They are merged into the entry walk in identity order,
//! which is what keeps the order the answer's. The caller re-tests the
//! condition, as it does on every index path, so a stale entry or a pending
//! record that does not hold the value is dropped there.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{
    IndexAddress, IndexValues, KeyKind, RecordValue, SecondaryIndexKey, StoreKey,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, Value};

use super::super::address::{after, resuming_after};
use super::super::{RecordAddress, Transaction};
use super::FIRST_FETCH_BATCH;
use crate::catalog::IndexDefinition;

impl Transaction<'_> {
    /// The records holding exactly `values` in a secondary index, handed over in
    /// record order until `hand` says to stop.
    pub(super) fn walk_complete_equality<F, E>(
        &mut self,
        index: &IndexDefinition,
        values: &[Value],
        mut hand: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(&mut Self, RecordId, Vec<u8>) -> std::result::Result<ControlFlow<()>, E>,
        E: From<crate::error::Error>,
    {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let mut from = address.prefix(KeyKind::SecondaryIndex);
        from.extend_from_slice(&IndexValues::leading(values));
        let end = after(from.clone());
        // Copied out before the walk: `hand` takes the transaction, so nothing
        // may hold a borrow of it across the call.
        let mut pending: BTreeMap<RecordId, Option<Vec<u8>>> = self
            .writes
            .iter()
            .filter(|(at, _)| {
                at.namespace == index.namespace
                    && at.database == index.database
                    && at.table == index.table
            })
            .map(|(at, held)| {
                let payload = match held {
                    RecordValue::Present(payload) => Some(payload.clone()),
                    RecordValue::Tombstone => None,
                };
                (at.id.clone(), payload)
            })
            .collect();

        let mut batch = FIRST_FETCH_BATCH;
        loop {
            let request = ScanRequest {
                keyspace: SecondaryIndexKey::keyspace(),
                range: KeyRange::between(Key::from(from.clone()), Key::from(end.clone())),
                direction: ScanDirection::Forward,
                limit: Some(batch),
            };
            let entries = self
                .store
                .backend()
                .scan(&request)
                .map_err(crate::error::Error::from)?;
            let full = entries.len() >= batch;
            let mut named = Vec::with_capacity(entries.len());
            for (key, _) in &entries {
                named.push(
                    SecondaryIndexKey::decode(key.as_slice())
                        .map_err(crate::error::Error::from)?
                        .id,
                );
            }
            // This round answers every identity up to the last entry it read —
            // or, once the entries are exhausted, every pending one left.
            let through = if full { named.last().cloned() } else { None };
            let mut round: BTreeMap<RecordId, Option<Option<Vec<u8>>>> =
                named.into_iter().map(|id| (id, None)).collect();
            let waiting: Vec<RecordId> = pending
                .keys()
                .filter(|id| through.as_ref().is_none_or(|last| *id <= last))
                .cloned()
                .collect();
            for id in waiting {
                if let Some(payload) = pending.remove(&id) {
                    round.insert(id, Some(payload));
                }
            }
            // The stored records this round names, resolved together.
            let stored: Vec<RecordAddress> = round
                .iter()
                .filter(|(_, written)| written.is_none())
                .map(|(id, _)| {
                    RecordAddress::new(index.namespace, index.database, index.table, id.clone())
                })
                .collect();
            let mut fetched = stored
                .iter()
                .map(|at| at.id.clone())
                .zip(self.get_each(&stored)?)
                .collect::<BTreeMap<_, _>>();
            for (id, written) in round {
                let payload = match written {
                    Some(payload) => payload,
                    None => fetched.remove(&id).flatten(),
                };
                let Some(payload) = payload else {
                    continue;
                };
                if hand(self, id, payload)?.is_break() {
                    return Ok(());
                }
            }
            let Some((last, _)) = entries.last().filter(|_| full) else {
                return Ok(());
            };
            from = resuming_after(last.as_slice().to_vec());
            batch = batch.saturating_mul(2).min(RANGE_SCAN_BATCH_ENTRIES);
        }
    }
}
