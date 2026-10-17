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

use tessari_encoding::{IndexAddress, IndexValues, KeyKind, RecordValue};
use tessari_types::{RecordId, Value};

use super::super::Transaction;
use super::super::address::after;
use super::super::walk::{EqualityWalk, Place, RecordWalk};
use super::FIRST_FETCH_BATCH;
use crate::catalog::IndexDefinition;

impl Transaction<'_> {
    /// The records holding exactly `values` in a secondary index, walked in
    /// record order a round at a time.
    pub(super) fn walk_complete_equality(
        &self,
        index: &IndexDefinition,
        values: &[Value],
    ) -> RecordWalk {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let mut from = address.prefix(KeyKind::SecondaryIndex);
        from.extend_from_slice(&IndexValues::leading(values));
        let end = after(from.clone());
        // Copied out when the walk begins: the caller holds the transaction
        // between rounds and may write through it.
        let pending: BTreeMap<RecordId, Option<Vec<u8>>> = self
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
        RecordWalk::equality(EqualityWalk {
            place: Place {
                namespace: index.namespace,
                database: index.database,
                table: index.table,
            },
            pending,
            from: Some(from),
            end,
            batch: FIRST_FETCH_BATCH,
        })
    }
}
