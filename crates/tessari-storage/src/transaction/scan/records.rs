//! Reading a table's records, with this transaction's pending writes settled in.

use super::super::address::{after, resuming_after};
use super::super::{RecordAddress, Transaction};
use super::{Reading, Walk};
use crate::error::Result;
use std::collections::BTreeMap;
use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{RecordKey, RecordValue, StampedValue, StoreKey, StoreValue};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

impl Transaction<'_> {
    pub(crate) fn table_records(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        walk: Walk<'_>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.records_of(namespace, database, table, walk)
    }

    /// The walk all four of the readers above share.
    pub(crate) fn records_of(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        walk: Walk<'_>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let Walk {
            bound,
            anchor,
            span,
            from: window_from,
            to: window_to,
            reading,
        } = walk;
        let prefix = RecordKey::table_prefix(namespace, database, table);
        let of_this_table = |address: &RecordAddress| {
            address.namespace == namespace && address.database == database && address.table == table
        };
        // The anchor's own versions are behind the page, not in it, so the walk
        // begins past the last of them rather than at the first.
        // Three ways to start, and they differ by one record. A span opens **at**
        // its lower bound because that bound is inside it; a cursor opens past
        // its anchor's own versions because it has already handed that record
        // over; and a plain walk opens at the table.
        let opening = match (&span, anchor) {
            (Some(span), _) => RecordKey::versions_prefix(namespace, database, table, span.lower),
            (None, Some(anchor)) => after(RecordKey::versions_prefix(
                namespace, database, table, anchor,
            )),
            (None, None) => prefix.clone(),
        };
        // The retention floor raises where the walk opens rather than filtering
        // what it passes, which is the whole reason a series table's identity
        // carries the millisecond: the scan **starts later** and never reads the
        // records it would have discarded. Taken as a maximum, so a span or a
        // cursor already past the floor is not pulled backwards by it.
        // A window's lower end raises the opening exactly as the floor below
        // does: a cursor already past it is not pulled back to it.
        let opening = match window_from {
            Some(lower) => {
                let at = RecordKey::versions_prefix(namespace, database, table, lower);
                if at > opening { at } else { opening }
            }
            None => opening,
        };
        let floor = self.series_floor(namespace, table)?;
        let opening = match &floor {
            Some(floor) => {
                let at = RecordKey::versions_prefix(namespace, database, table, floor);
                if at > opening { at } else { opening }
            }
            None => opening,
        };
        let wanted = bound.map(|bound| {
            let displacing = self
                .writes
                .iter()
                .filter(|(address, value)| {
                    of_this_table(address) && matches!(value, RecordValue::Tombstone)
                })
                .count();
            bound.saturating_add(displacing)
        });

        // Versions of one record are adjacent and sort newest-first, so the
        // first version at or before the snapshot is the visible one and every
        // later entry for that record is an older version to walk past.
        //
        // `resolved` carries across batches for that reason: a record's versions
        // may straddle a boundary, and forgetting which record was just settled
        // would let an older version of it be read as a newer record.
        let mut live: BTreeMap<RecordId, RecordValue> = BTreeMap::new();
        let mut resolved: Option<RecordId> = None;
        // Counted separately from `live.len()`, which includes the tombstones
        // that are about to be filtered out. Stopping on a count that includes
        // them would answer short by however many deleted records the walk
        // happened to pass.
        let mut present = 0_usize;
        let mut from = opening;
        let end = match &span {
            // An exclusive upper bound stops **before** that record's first
            // version; an inclusive one stops after its last.
            Some(span) => {
                let at = RecordKey::versions_prefix(namespace, database, table, span.upper);
                if span.inclusive { after(at) } else { at }
            }
            None => match window_to {
                Some((upper, inclusive)) => {
                    let at = RecordKey::versions_prefix(namespace, database, table, upper);
                    if inclusive { after(at) } else { at }
                }
                None => after(prefix),
            },
        };
        loop {
            let request = ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::between(Key::from(from), Key::from(end.clone())),
                direction: ScanDirection::Forward,
                // The unbounded read stays one scan of everything, unchanged.
                // Only a read that knows what it needs asks for less, and it
                // asks for exactly what it still needs so a small bound costs a
                // small scan rather than a batch-sized one.
                limit: wanted.map(|wanted| {
                    wanted
                        .saturating_sub(present)
                        .clamp(1, RANGE_SCAN_BATCH_ENTRIES)
                }),
            };
            let batch = match reading {
                Reading::Serving => self.store.backend().scan(&request)?,
                Reading::Sweep => self.store.backend().sweep(&request)?,
            };
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            let entries = batch.len();
            for (id, value) in self.settled(batch, &mut resolved)? {
                if matches!(value, RecordValue::Present(_)) {
                    present = present.saturating_add(1);
                }
                live.insert(id, value);
            }
            let Some(wanted) = wanted else {
                break;
            };
            // A batch shorter than asked for is the end of the table; otherwise
            // the walk continues until it has what it came for.
            let Some(last) = last.filter(|_| present < wanted && entries > 0) else {
                break;
            };
            from = resuming_after(last);
        }

        for (address, value) in &self.writes {
            // A pending write is folded in only where the committed walk would
            // have reached it. Without the second test a record written but not
            // yet committed would appear on a page it sorts before, which is the
            // one way a cursor could answer with a record it had already handed
            // the caller.
            // And a span applies to a pending write for the same reason the
            // cursor test does: a record written but not committed still has to
            // be outside a span it is outside of, or a bounded read answers with
            // a record the same read would not have found a moment earlier.
            // A pending write is judged against the floor for the same reason
            // it is judged against the span: a record this transaction wrote
            // below the floor is a record the same read would not have found a
            // moment earlier, and returning it would make the floor a property
            // of who is asking.
            if of_this_table(address)
                && floor.as_ref().is_none_or(|floor| &address.id >= floor)
                && anchor.is_none_or(|anchor| &address.id > anchor)
                && span.as_ref().is_none_or(|span| span.holds(&address.id))
                && window_from.is_none_or(|lower| &address.id >= lower)
                && window_to.is_none_or(|(upper, inclusive)| {
                    if inclusive {
                        &address.id <= upper
                    } else {
                        &address.id < upper
                    }
                })
            {
                live.insert(address.id.clone(), value.clone());
            }
        }

        Ok(live
            .into_iter()
            .filter_map(|(id, value)| match value {
                RecordValue::Present(payload) => Some((id, payload)),
                RecordValue::Tombstone => None,
            })
            .collect())
    }

    /// Which records a batch of raw entries settles, in the order it holds them.
    ///
    /// Versions of one record are adjacent and sort newest-first, so the first
    /// entry at or before the snapshot is the visible one and every later entry
    /// for that record is an older version to walk past. `resolved` is the
    /// caller's because a record's versions may straddle a batch boundary, and
    /// forgetting which record was just settled would let an older version of it
    /// be read as a newer record.
    ///
    /// Asked by both the collecting walk and the streaming one. The two answer
    /// different shapes and must not disagree about which version a reader sees;
    /// two copies of that rule would be two places for it to drift.
    pub(crate) fn settled(
        &self,
        batch: Vec<(Key, tessari_kv::Value)>,
        resolved: &mut Option<RecordId>,
    ) -> Result<Vec<(RecordId, RecordValue)>> {
        let mut taken = Vec::with_capacity(batch.len());
        for (key, value) in batch {
            let decoded = RecordKey::decode(key.as_slice())?;
            if decoded.version > self.snapshot || resolved.as_ref() == Some(&decoded.id) {
                continue;
            }
            *resolved = Some(decoded.id.clone());
            taken.push((
                decoded.id,
                StampedValue::decode(value.as_slice())?.into_visible_at(self.reading_at()),
            ));
        }
        Ok(taken)
    }

    /// One batch of raw entries from a span of the record keyspace.
    ///
    /// Its own method so that a caller whose error type is not this crate's can
    /// still write `?` over the scan: everything fallible about the walk is on
    /// this side of the boundary, and the callback's side converts once.
    pub(crate) fn batch_of(
        &self,
        from: &[u8],
        end: &[u8],
        limit: Option<usize>,
    ) -> Result<Vec<(Key, tessari_kv::Value)>> {
        Ok(self.store.backend().scan(&ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::between(Key::from(from.to_vec()), Key::from(end.to_vec())),
            direction: ScanDirection::Forward,
            limit,
        })?)
    }
}
