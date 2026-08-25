//! Reads that come back in an order the index already holds.
//!
//! An ordered read walks index entries rather than records, so the order costs
//! nothing to produce — and the group boundary is what tells the walk when a
//! leading field has changed underneath it.

use tessari_constants::ORDERED_SCAN_BATCH_ENTRIES;
use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, SecondaryIndexKey, StoreKey, StoreValue,
    UniqueIndexKey,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::RecordId;

use super::address::{after, resuming_after};
use super::{RecordAddress, StoredRecord, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

impl Transaction<'_> {
    /// How far into `entries[at..end]` the tie group `edge` still runs.
    ///
    /// Both walks ask this the same way and for the same reason: a drain reads
    /// exactly the records still in the group and stops, so where the group ends
    /// has to be answered from the **keys** — before a single record is fetched.
    /// A record could answer it too, but only by reading the rows the question
    /// exists to avoid reading.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry's values cannot be walked.
    fn still_in_group(
        &self,
        entries: &[(IndexValues, RecordId)],
        at: usize,
        end: usize,
        leading_fields: usize,
        edge: &[u8],
    ) -> Result<usize> {
        let mut stop = at;
        while stop < end {
            let Some((values, _)) = entries.get(stop) else {
                break;
            };
            if values.leading_of(leading_fields)? != edge {
                break;
            }
            stop = stop.saturating_add(1);
        }
        Ok(stop)
    }

    /// The records an index holds, **greatest value first**, stopping once the
    /// bound is filled and its tie group closed.
    ///
    /// `None` when the index runs out before `wanted` records were found. That
    /// is not an error and not an empty answer: the records an index does not
    /// hold — the ones whose indexed value is absent — sort *below* every value
    /// it does hold, so an answer the index cannot fill needs them, and finding
    /// them is the scan. The caller falls back to it.
    ///
    /// # Why the tie group is drained
    ///
    /// The order a read answers in is the value system's order with ties broken
    /// by the record's identity **ascending**, and an entry's key is its value
    /// followed by that identity — so walking backwards yields a tie group with
    /// its identities *descending*. Cutting the walk at the bound would therefore
    /// take the wrong members of the group straddling it: ten records sharing one
    /// value under `LIMIT 10` would answer with the ten largest identities where
    /// the order asks for the ten smallest.
    ///
    /// So the walk continues past the bound until an entry carries a different
    /// value, and the caller sorts and cuts what comes back. Two properties make
    /// the comparison exact rather than approximate: the encoding is
    /// order-preserving, and it **normalises** — `1` and `1.0` encode
    /// identically, which is the same pair the value system's order calls equal.
    /// A tie group in bytes is a tie group in the order.
    ///
    /// The walk is unbounded only when the ordering is: a table whose every
    /// record carries one value costs the whole index, which is what ordering by
    /// a constant is.
    ///
    /// # Entries are taken at face value here, and that is a precondition
    ///
    /// Every other index read in this type treats an entry as a candidate and
    /// confirms it against the record, because entries hold the current state and
    /// carry no version. This one does not, because it has no condition to
    /// confirm against — the entry's *position* is the answer. Its caller
    /// therefore serves an ordering only from the committed tail, where every
    /// entry does reflect the record it points at.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_in_descending_order(
        &self,
        index: &IndexDefinition,
        leading_fields: usize,
        wanted: usize,
    ) -> Result<Option<Vec<StoredRecord>>> {
        if wanted == 0 {
            return Ok(Some(Vec::new()));
        }
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = address.prefix(kind);
        let lower = Key::from(prefix.clone());
        let mut upper = Key::from(after(prefix));
        let mut found: Vec<StoredRecord> = Vec::new();
        // The leading values of the `wanted`-th record's entry, once there is
        // one. From then on the walk is draining a tie group rather than filling
        // a bound. Descending drains whatever the order names: walking backwards
        // reverses the group's inner order, so even a group ordered by identity
        // comes out in the reverse of the answer's order.
        let mut boundary: Option<Vec<u8>> = None;
        loop {
            let request = ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::between(lower.clone(), upper.clone()),
                direction: ScanDirection::Reverse,
                limit: Some(ORDERED_SCAN_BATCH_ENTRIES),
            };
            let batch = self.store.backend().scan(&request)?;
            let last = batch.last().map(|(key, _)| key.clone());
            // Decoded before anything is resolved, because deciding *what* to
            // resolve reads the entry's key and never its record: the boundary
            // test asks whether an entry is still in the tie group, and the
            // group is the value the key carries. So the walk can plan a whole
            // batch's reads without issuing one.
            let mut entries: Vec<(IndexValues, RecordId)> = Vec::with_capacity(batch.len());
            for (key, value) in &batch {
                entries.push(if index.unique {
                    (
                        UniqueIndexKey::decode(key.as_slice())?.values,
                        IndexTarget::decode(value.as_slice())?.id,
                    )
                } else {
                    let entry = SecondaryIndexKey::decode(key.as_slice())?;
                    (entry.values, entry.id)
                });
            }
            let mut at = 0;
            while at < entries.len() {
                // Sized to what is still needed rather than to the scan batch.
                // Resolving the whole batch would be one round trip instead of
                // ten and a hundred and twenty-eight record reads instead of
                // ten — a different cost, not a smaller one. At most one chunk
                // of overhang is read past the point the bound fills, and that
                // is bounded by `wanted`.
                let still = wanted.saturating_sub(found.len()).max(1);
                let mut end = at.saturating_add(still).min(entries.len());
                if let Some(edge) = &boundary {
                    // Draining a tie group, not filling a bound. Where it ends
                    // is knowable from the keys, so the drain reads exactly the
                    // records still in the group and stops.
                    end = self.still_in_group(&entries, at, end, leading_fields, edge)?;
                    if end == at {
                        return Ok(Some(found));
                    }
                }
                let chunk = entries.get(at..end).unwrap_or_default();
                let addresses: Vec<RecordAddress> = chunk
                    .iter()
                    .map(|(_, id)| {
                        RecordAddress::new(index.namespace, index.database, index.table, id.clone())
                    })
                    .collect();
                for ((values, id), payload) in chunk.iter().zip(self.get_each(&addresses)?) {
                    if let Some(edge) = &boundary
                        && values.leading_of(leading_fields)? != edge.as_slice()
                    {
                        return Ok(Some(found));
                    }
                    if let Some(payload) = payload {
                        found.push((id.clone(), payload));
                        if found.len() >= wanted && boundary.is_none() {
                            boundary = Some(values.leading_of(leading_fields)?.to_vec());
                        }
                    }
                }
                at = end;
            }
            // A short batch is the end of the index: the walk has seen every
            // entry, and whether that filled the bound is the whole answer.
            let Some(last) = last.filter(|_| batch.len() >= ORDERED_SCAN_BATCH_ENTRIES) else {
                return Ok((found.len() >= wanted).then_some(found));
            };
            // The upper end is exclusive, so the next batch continues strictly
            // below the last entry this one read.
            upper = last;
        }
    }

    /// The records an index holds, **least value first**, stopping once the
    /// bound is filled.
    ///
    /// # Precondition: every record of the table has an entry
    ///
    /// A record whose indexed value is absent has **no index entry**, and the
    /// value system puts `none` below every value — so ascending, the records an
    /// index does not hold are exactly the ones that come *first*. This walk
    /// cannot see them and does not try to. Its caller admits the read only over
    /// a field the schema declares `REQUIRED`, where there are no absences: the
    /// declaration is refused against a table already holding a record without
    /// the field, and every write is checked after it, so the invariant holds in
    /// both directions in time.
    ///
    /// Under that precondition an index that runs out has answered the **whole
    /// table**, so a short answer is a complete one and this returns it rather
    /// than `None`. That is the difference from
    /// [`Transaction::records_in_descending_order`], which must hand a short
    /// answer back for the scan to finish.
    ///
    /// # Why there is no tie group to drain
    ///
    /// The order a read answers in is the value system's order with ties broken
    /// by the record's identity **ascending**, and an entry's key is its value
    /// followed by that identity. A forward walk therefore yields a tie group
    /// with its identities ascending — which is already the order the answer
    /// wants, so the first `wanted` entries are the first `wanted` records.
    /// Descending has to drain past the bound precisely because walking
    /// backwards reverses that inner order; ascending does not reverse it, and
    /// the asymmetry is in the direction rather than in the rule.
    ///
    /// # Entries are taken at face value here, and that is a precondition too
    ///
    /// As in the descending walk: the entry's *position* is the answer and there
    /// is no condition to confirm it against, so the caller serves an ordering
    /// only from the committed tail. A record the reader cannot see is skipped
    /// rather than counted, which is why the walk continues until the bound is
    /// filled instead of reading exactly `wanted` entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_in_ascending_order(
        &self,
        index: &IndexDefinition,
        leading_fields: usize,
        wanted: usize,
    ) -> Result<Vec<StoredRecord>> {
        if wanted == 0 {
            return Ok(Vec::new());
        }
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = address.prefix(kind);
        let mut lower = Key::from(prefix.clone());
        let upper = Key::from(after(prefix));
        // The order names `leading_fields` of the index's fields. Where it names
        // *all* of them the tie group's inner order is the record's identity,
        // ascending — the answer's own order — so the first `wanted` entries are
        // the first `wanted` records and there is nothing to drain. Where it
        // names fewer, the group is ordered by the *next* indexed field instead,
        // and cutting at the bound would take the wrong members of it.
        let drains = leading_fields < index.fields.len();
        let mut found: Vec<StoredRecord> = Vec::new();
        let mut boundary: Option<Vec<u8>> = None;
        loop {
            let request = ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::between(lower.clone(), upper.clone()),
                direction: ScanDirection::Forward,
                limit: Some(ORDERED_SCAN_BATCH_ENTRIES),
            };
            let batch = self.store.backend().scan(&request)?;
            let last = batch.last().map(|(key, _)| key.clone());
            let mut entries: Vec<(IndexValues, RecordId)> = Vec::with_capacity(batch.len());
            for (key, value) in &batch {
                entries.push(if index.unique {
                    (
                        UniqueIndexKey::decode(key.as_slice())?.values,
                        IndexTarget::decode(value.as_slice())?.id,
                    )
                } else {
                    let entry = SecondaryIndexKey::decode(key.as_slice())?;
                    (entry.values, entry.id)
                });
            }
            let mut at = 0;
            while at < entries.len() {
                // Sized to what is still needed rather than to the scan batch,
                // for the reason wave 34 recorded: resolving the whole batch
                // would take the round trips from ten to one *and* the record
                // reads from ten to a hundred and twenty-eight, which is a
                // different cost rather than a smaller one.
                let still = wanted.saturating_sub(found.len()).max(1);
                let mut end = at.saturating_add(still).min(entries.len());
                if let Some(edge) = &boundary {
                    end = self.still_in_group(&entries, at, end, leading_fields, edge)?;
                    if end == at {
                        return Ok(found);
                    }
                }
                let chunk = entries.get(at..end).unwrap_or_default();
                let addresses: Vec<RecordAddress> = chunk
                    .iter()
                    .map(|(_, id)| {
                        RecordAddress::new(index.namespace, index.database, index.table, id.clone())
                    })
                    .collect();
                for ((values, id), payload) in chunk.iter().zip(self.get_each(&addresses)?) {
                    if let Some(edge) = &boundary
                        && values.leading_of(leading_fields)? != edge.as_slice()
                    {
                        return Ok(found);
                    }
                    if let Some(payload) = payload {
                        found.push((id.clone(), payload));
                        if found.len() >= wanted {
                            if !drains {
                                return Ok(found);
                            }
                            if boundary.is_none() {
                                boundary = Some(values.leading_of(leading_fields)?.to_vec());
                            }
                        }
                    }
                }
                at = end;
            }
            // A short batch is the end of the index, and under this method's
            // precondition that is the end of the table.
            let Some(last) = last.filter(|_| batch.len() >= ORDERED_SCAN_BATCH_ENTRIES) else {
                return Ok(found);
            };
            // The lower end is inclusive, so the next batch continues strictly
            // above the last entry this one read. `resuming_after` and never
            // `after`: the latter is the successor of the whole *prefix* and
            // would skip every key carrying this one as a byte prefix, which
            // costs a walk records rather than an error.
            lower = Key::from(resuming_after(last.as_slice().to_vec()));
        }
    }
}
