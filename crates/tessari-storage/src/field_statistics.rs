//! What one declared field of one table holds, summarised for the planner,
//! and how much of the table has changed since.
//!
//! # The index statistics' sibling, same discipline
//!
//! [`crate::statistics`] summarises an index's entries; this summarises one
//! declared field of the table itself, for the questions an index cannot
//! answer — a predicate on a field nothing indexes. The rules carry over
//! unchanged: the walk reads this node's copy as of its snapshot and answers
//! a cost and never an answer; the summary sits in a keyspace derived from
//! the table address, in neither the log nor a backup; the change counter
//! moves in the same commit batch as the writes it counts; a statistic that
//! has drifted is set aside and the planner behaves as if none was taken.
//!
//! # Exact, and bounded by what is distinct
//!
//! The walk counts every value exactly, holding one counter per distinct
//! encoding — memory is bounded by what the field actually holds, not by how
//! many records hold it. The buckets and the common values are computed from
//! the sorted counters, so they are exact equi-depth, not sampled.

use std::collections::BTreeMap;

use tessari_constants::{
    PLANNER_SCAN_FLOOR_RECORDS, STATISTICS_BUCKETS, STATISTICS_COMMON_VALUES, STATISTICS_PER_PASS,
};
use tessari_encoding::{
    FieldAddress, FieldChanges, FieldChangesKey, FieldStatistics, FieldStatisticsKey, IndexValues,
    KeyKind, StoreKey, StoreValue, decode_payload,
};
use tessari_kv::WriteBatch;
use tessari_types::{DatabaseId, NamespaceId, Path, TableId, Value};

use crate::catalog::{Catalog, FieldDefinition};
use crate::error::Result;
use crate::statistics::still_fresh;
use crate::store::Store;
use crate::transaction::Transaction;

/// Add what one commit changed to each table's field-statistics change
/// counter, beside the records the changes are about.
///
/// Called with every record mutation the commit carried, whether or not the
/// table declares fields: a counter on a table nobody summarises is never
/// read, and deciding per table would cost a catalog read the write path
/// does not need.
///
/// # Errors
///
/// Returns an error when the backend fails or a held counter cannot be decoded.
pub(crate) async fn count_field_changes(
    store: &Store,
    mut batch: WriteBatch,
    changes: &BTreeMap<(NamespaceId, DatabaseId, TableId), u64>,
) -> Result<WriteBatch> {
    for ((namespace, database, table), counted) in changes {
        let key = FieldChangesKey::new(*namespace, *database, *table).encode();
        let held = match store
            .backend()
            .get(KeyKind::FieldChanges.keyspace(), &key)
            .await?
        {
            Some(bytes) => FieldChanges::decode(bytes.as_slice())?.0,
            None => 0,
        };
        batch = batch.put(
            KeyKind::FieldChanges.keyspace(),
            key,
            FieldChanges(held.saturating_add(*counted)).encode(),
        );
    }
    Ok(batch)
}

/// How many writes one table has gained or lost on this node, ever.
async fn changes_of(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
) -> Result<u64> {
    let key = FieldChangesKey::new(namespace, database, table).encode();
    Ok(
        match store
            .backend()
            .get(KeyKind::FieldChanges.keyspace(), &key)
            .await?
        {
            Some(bytes) => FieldChanges::decode(bytes.as_slice())?.0,
            None => 0,
        },
    )
}

async fn held_statistics(store: &Store, address: &FieldAddress) -> Result<Option<FieldStatistics>> {
    let key = FieldStatisticsKey::new(address.clone()).encode();
    match store
        .backend()
        .get(KeyKind::FieldStatistics.keyspace(), &key)
        .await?
    {
        Some(bytes) => Ok(Some(FieldStatistics::decode(bytes.as_slice())?)),
        None => Ok(None),
    }
}

fn address_of(field: &FieldDefinition) -> FieldAddress {
    FieldAddress::new(
        field.namespace,
        field.database,
        field.table,
        Path::field(&field.name),
    )
}

/// The running summary of one field, over one pass of a table.
///
/// Counters keyed by the field's order-preserving encoding, so sorting them
/// is sorting the values and every derived figure is exact.
struct Tally {
    seen: u64,
    nulls: u64,
    counts: BTreeMap<Vec<u8>, u64>,
}

impl Tally {
    fn new() -> Self {
        Self {
            seen: 0,
            nulls: 0,
            counts: BTreeMap::new(),
        }
    }

    fn count(&mut self, value: &Value) {
        self.seen = self.seen.saturating_add(1);
        let encoded = IndexValues::leading(core::slice::from_ref(value));
        let held = self.counts.entry(encoded).or_insert(0);
        *held = held.saturating_add(1);
    }

    fn null(&mut self) {
        self.seen = self.seen.saturating_add(1);
        self.nulls = self.nulls.saturating_add(1);
    }

    fn finish(self, records: u64, changes: u64) -> FieldStatistics {
        let entries = self.seen.saturating_sub(self.nulls);
        let distinct = u64::try_from(self.counts.len()).unwrap_or(u64::MAX);
        let low = self.counts.keys().next().cloned();
        let high = self.counts.keys().next_back().cloned();
        let mut common: Vec<(Vec<u8>, u64)> =
            self.counts.iter().map(|(k, v)| (k.clone(), *v)).collect();
        common.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        common.truncate(STATISTICS_COMMON_VALUES);
        let bounds = equi_depth(&self.counts, entries);
        FieldStatistics {
            records,
            changes,
            entries,
            nulls: self.nulls,
            distinct,
            low,
            high,
            common,
            bounds,
        }
    }
}

/// Equi-depth bucket bounds over the sorted counters: a bound every
/// `entries / buckets` records, so each pair of neighbours holds about the
/// same share. The last value is always a bound.
fn equi_depth(counts: &BTreeMap<Vec<u8>, u64>, entries: u64) -> Vec<Vec<u8>> {
    let buckets = u64::try_from(STATISTICS_BUCKETS).unwrap_or(u64::MAX);
    if entries == 0 || counts.len() < 2 {
        return counts.keys().cloned().collect();
    }
    let target = entries.checked_div(buckets).unwrap_or(entries).max(1);
    let mut bounds = Vec::new();
    let mut cumulative = 0_u64;
    for (value, count) in counts {
        cumulative = cumulative.saturating_add(*count);
        let next = u64::try_from(bounds.len().saturating_add(1)).unwrap_or(u64::MAX);
        if cumulative >= target.saturating_mul(next) {
            bounds.push(value.clone());
        }
    }
    if let Some(last) = counts.keys().next_back()
        && bounds.last() != Some(last)
    {
        bounds.push(last.clone());
    }
    bounds
}

impl Transaction<'_> {
    /// Walk one table for one declared field and summarise what it held, and
    /// keep the summary on this node.
    ///
    /// The walk reads the table once through [`Self::sweep_table`] — the read
    /// a pass that will not read the records again uses, so the block cache
    /// is not filled with a table nobody is serving. Values are counted by
    /// their order-preserving encoding: nothing is decoded that the statistic
    /// does not compare.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a record cannot be decoded.
    pub async fn analyze_field(
        &self,
        field: &FieldDefinition,
        records: u64,
    ) -> Result<FieldStatistics> {
        let address = address_of(field);
        let changes =
            changes_of(self.store(), field.namespace, field.database, field.table).await?;
        let path = Path::field(&field.name);
        let mut tally = Tally::new();
        for (_, bytes) in self
            .sweep_table(field.namespace, field.database, field.table)
            .await?
        {
            let record = decode_payload(&bytes)?;
            match path.resolve(&record) {
                // `Null` is what a stored null reads back as; `None` is the
                // internal absence a decoder can meet inside a record. Both
                // are "no value" to a statistic.
                Some(Value::Null) | Some(Value::None) => tally.null(),
                Some(held) => tally.count(held),
                None => {}
            }
        }
        let statistics = tally.finish(records, changes);
        self.store()
            .backend()
            .apply(WriteBatch::new().put(
                KeyKind::FieldStatistics.keyspace(),
                FieldStatisticsKey::new(address).encode(),
                statistics.encode(),
            ))
            .await?;
        Ok(statistics)
    }

    /// Summarise every declared field of one table, in the catalog's order.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog, the backend or a record cannot be
    /// read.
    pub async fn analyze_table_fields(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<(FieldDefinition, FieldStatistics)>> {
        let records = Catalog::new(self).record_count(table).await?.unwrap_or(0);
        let fields: Vec<FieldDefinition> = Catalog::new(self)
            .fields()
            .await?
            .into_iter()
            .filter(|field| {
                field.namespace == namespace && field.database == database && field.table == table
            })
            .collect();
        let mut taken = Vec::with_capacity(fields.len());
        for field in &fields {
            let statistics = self.analyze_field(field, records).await?;
            taken.push((field.clone(), statistics));
        }
        Ok(taken)
    }

    /// One field's statistic as held on this node, fresh or not, with the
    /// table's change counter now — what a report that names staleness needs.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a held value cannot be
    /// decoded.
    pub async fn held_field_statistics(
        &self,
        field: &FieldDefinition,
    ) -> Result<Option<(FieldStatistics, u64)>> {
        let address = address_of(field);
        let Some(statistics) = held_statistics(self.store(), &address).await? else {
            return Ok(None);
        };
        let now = changes_of(self.store(), field.namespace, field.database, field.table).await?;
        Ok(Some((statistics, now)))
    }

    /// One field's statistic, when there is one and it still describes the
    /// table.
    ///
    /// `None` both when nothing was ever taken and when more has changed
    /// since than the statistic can absorb — the two cases a planner treats
    /// alike.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a held value cannot be
    /// decoded.
    pub async fn fresh_field_statistics(
        &self,
        field: &FieldDefinition,
    ) -> Result<Option<FieldStatistics>> {
        let address = address_of(field);
        let Some(statistics) = held_statistics(self.store(), &address).await? else {
            return Ok(None);
        };
        let now = changes_of(self.store(), field.namespace, field.database, field.table).await?;
        Ok(still_fresh(statistics.changes, statistics.entries, now).then_some(statistics))
    }
}

impl Store {
    /// Take the statistics of fields that have none or whose statistic has
    /// gone stale, at most [`STATISTICS_PER_PASS`] of them, on tables large
    /// enough for the planner to weigh them.
    ///
    /// Answers how many were taken. This node's own work, like the index
    /// statistics: every node keeps its own, leader or not.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the catalog cannot be read.
    pub async fn refresh_field_statistics(&self) -> Result<usize> {
        let transaction = self.begin().await?;
        let mut stale = Vec::new();
        {
            let mut reading = self.begin().await?;
            let mut catalog = Catalog::new(&mut reading);
            'tables: for namespace in catalog.namespaces().await? {
                for database in catalog.databases_in(namespace.id).await? {
                    for table in catalog.tables_in(namespace.id, database.id).await? {
                        let Some(records) = catalog.record_count(table.id).await? else {
                            continue;
                        };
                        if records < PLANNER_SCAN_FLOOR_RECORDS {
                            continue;
                        }
                        for field in catalog.fields().await? {
                            if field.namespace != namespace.id
                                || field.database != database.id
                                || field.table != table.id
                            {
                                continue;
                            }
                            let fresh = {
                                let address = address_of(&field);
                                match held_statistics(self, &address).await? {
                                    Some(held) => {
                                        let now = changes_of(
                                            self,
                                            field.namespace,
                                            field.database,
                                            field.table,
                                        )
                                        .await?;
                                        still_fresh(held.changes, held.entries, now)
                                    }
                                    None => false,
                                }
                            };
                            if fresh {
                                continue;
                            }
                            stale.push((field, records));
                            if stale.len() >= STATISTICS_PER_PASS {
                                break 'tables;
                            }
                        }
                    }
                }
            }
        }
        for (field, records) in &stale {
            transaction.analyze_field(field, *records).await?;
        }
        Ok(stale.len())
    }
}

#[cfg(test)]
mod tests {
    use super::equi_depth;
    use std::collections::BTreeMap;

    fn skewed(total: u64, weights: &[(u8, u64)]) -> BTreeMap<Vec<u8>, u64> {
        // `weights` as (value-byte, share-in-tenths).
        weights
            .iter()
            .map(|(value, tenths)| (vec![*value], total.saturating_mul(*tenths) / 10))
            .collect()
    }

    #[test]
    fn equi_depth_bounds_split_by_count_not_by_value() {
        // 10k records across three values with a zipf-ish skew: the middle
        // value is alone in the middle buckets.
        let counts = skewed(10_000, &[(b'a', 5), (b'b', 3), (b'c', 2)]);
        let bounds = equi_depth(&counts, 10_000);
        assert_eq!(
            bounds.first(),
            Some(&vec![b'a']),
            "the smallest value opens"
        );
        assert_eq!(bounds.last(), Some(&vec![b'c']), "the largest value closes");
        // Every bound is a value that exists — an equi-depth bound between
        // real values would be a value the field never held.
        for bound in &bounds {
            assert!(counts.contains_key(bound), "bounds are held values");
        }
    }

    #[test]
    fn a_single_distinct_value_is_its_own_only_bound() {
        let counts: BTreeMap<Vec<u8>, u64> = BTreeMap::from([(vec![b'x'], 9_999)]);
        assert_eq!(equi_depth(&counts, 9_999), vec![vec![b'x']]);
    }

    #[test]
    fn empty_input_has_no_bounds() {
        let counts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        assert!(equi_depth(&counts, 0).is_empty());
    }
}
