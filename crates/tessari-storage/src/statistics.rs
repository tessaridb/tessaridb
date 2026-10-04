//! What a value index holds, summarised for the planner.
//!
//! # Why the planner needs more than a count
//!
//! The planner kept one number per table — how many records it holds
//! ([`crate::cardinality`]) — and decided whether an index beats the table by
//! **counting** the winner's entries up to half the table on every read. That
//! count is exact and it is the cost of the read twice over in the case it
//! exists for: an index that selects most of a table pays a walk to half of it
//! before the planner declines it. And it says nothing about two indexes that
//! could each serve a read, so the planner ranked those by shape alone.
//!
//! A statistic is that count taken once instead of per read, with enough beside
//! it to answer the questions a read asks: how many entries one value holds
//! (the common values, then the spread of the rest), how many a leading run of
//! the fields narrows to (distinct values per run), and how many fall between
//! two bounds of the first field (equi-depth buckets).
//!
//! # What a statistic is not
//!
//! It decides which access path a read takes and never which records it
//! returns. So it is taken from the entries this node holds and kept beside
//! them outside the log, like a vector index's measured recall: a replica
//! walks its own copy, and two nodes holding different statistics answer the
//! same query by different paths and with the same records. A statistic that
//! has drifted is set aside rather than trusted — see
//! [`Transaction::fresh_statistics`] — and the planner counts instead, as it
//! did before statistics existed.

use std::collections::BTreeMap;

use tessari_constants::{
    PLANNER_SCAN_FLOOR_RECORDS, RANGE_SCAN_BATCH_ENTRIES, STATISTICS_BUCKETS,
    STATISTICS_COMMON_VALUES, STATISTICS_PER_PASS, STATISTICS_STALE_FLOOR_CHANGES,
};
use tessari_encoding::{
    IndexAddress, IndexChanges, IndexChangesKey, IndexStatistics, IndexStatisticsKey, IndexValues,
    KeyKind, SecondaryIndexKey, StoreKey, StoreValue,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::Value;

use crate::catalog::{Catalog, IndexDefinition};
use crate::error::Result;
use crate::store::Store;
use crate::transaction::Transaction;

/// Whether an index is one the planner keeps statistics for.
///
/// A plain value index. A unique one already promises at most one record per
/// complete value, a search index ranks by its own collection statistics, and a
/// vector or spatial index is read by a walk or a covering rather than by a
/// value — none of them has a question a value statistic answers.
#[must_use]
pub(crate) fn kept(index: &IndexDefinition) -> bool {
    !index.unique
        && !index.search
        && !index.spatial
        && index.vector.is_none()
        && index.engine.is_none()
}

/// Add what one commit changed to each index's change counter.
///
/// Read from the backend and written into the commit's own batch, beside the
/// entries the changes are about, so the counter moves exactly when they do.
/// Saturating: the counter only decides when a statistic is set aside.
///
/// # Errors
///
/// Returns an error when the backend fails or a held counter cannot be decoded.
pub(crate) fn count_changes(
    store: &Store,
    mut batch: WriteBatch,
    changes: &BTreeMap<IndexAddress, u64>,
) -> Result<WriteBatch> {
    for (address, changed) in changes {
        let key = IndexChangesKey::new(*address).encode();
        let held = match store.backend().get(IndexChangesKey::keyspace(), &key)? {
            Some(bytes) => IndexChanges::decode(bytes.as_slice())?.0,
            None => 0,
        };
        batch = batch.put(
            IndexChangesKey::keyspace(),
            key,
            IndexChanges(held.saturating_add(*changed)).encode(),
        );
    }
    Ok(batch)
}

/// How many entries of one index have changed on this node, ever.
fn changes_of(store: &Store, address: IndexAddress) -> Result<u64> {
    let key = IndexChangesKey::new(address).encode();
    Ok(
        match store.backend().get(IndexChangesKey::keyspace(), &key)? {
            Some(bytes) => IndexChanges::decode(bytes.as_slice())?.0,
            None => 0,
        },
    )
}

fn address_of(index: &IndexDefinition) -> IndexAddress {
    IndexAddress::new(index.namespace, index.database, index.table, index.id)
}

/// The statistic one index held, as read from this node.
fn held_statistics(store: &Store, address: IndexAddress) -> Result<Option<IndexStatistics>> {
    let key = IndexStatisticsKey::new(address).encode();
    match store.backend().get(IndexStatisticsKey::keyspace(), &key)? {
        Some(bytes) => Ok(Some(IndexStatistics::decode(bytes.as_slice())?)),
        None => Ok(None),
    }
}

/// Whether a statistic taken at `at` changes still describes an index that has
/// seen `now`.
fn still_fresh(statistics: &IndexStatistics, now: u64) -> bool {
    let since = now.saturating_sub(statistics.changes);
    since <= (statistics.entries / 10).max(STATISTICS_STALE_FLOOR_CHANGES)
}

impl Transaction<'_> {
    /// Walk one index's entries and summarise them, and keep the summary on
    /// this node.
    ///
    /// The walk reads keys and no records. It reads the index as committed —
    /// this transaction's own writes have no entries yet and are not counted —
    /// and `records` is the table's count to scale an estimate by later.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or an entry cannot be decoded.
    pub fn analyze_index(&self, index: &IndexDefinition, records: u64) -> Result<IndexStatistics> {
        let address = address_of(index);
        let changes = changes_of(self.store(), address)?;
        let arity = index.fields.len().max(1);
        let prefix = address.prefix(KeyKind::SecondaryIndex);
        let end = crate::transaction::after(prefix.clone());

        let mut walk = Walk::new(arity);
        let mut from = prefix;
        loop {
            let request = ScanRequest {
                keyspace: SecondaryIndexKey::keyspace(),
                range: KeyRange::between(Key::from(from.clone()), Key::from(end.clone())),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES),
            };
            let entries = self.store().backend().scan(&request)?;
            for (key, _) in &entries {
                walk.take(&SecondaryIndexKey::decode(key.as_slice())?.values)?;
            }
            let Some((last, _)) = entries
                .last()
                .filter(|_| entries.len() >= RANGE_SCAN_BATCH_ENTRIES)
            else {
                break;
            };
            from = crate::transaction::resuming_after(last.as_slice().to_vec());
        }
        let statistics = walk.finish(records, changes);
        self.store().backend().apply(WriteBatch::new().put(
            IndexStatisticsKey::keyspace(),
            IndexStatisticsKey::new(address).encode(),
            statistics.encode(),
        ))?;
        Ok(statistics)
    }

    /// Take the statistics of every value index on one table.
    ///
    /// Answers each index summarised with what was taken, in the catalog's
    /// order; an index this store keeps no statistic for is left out.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog or the backend cannot be read.
    pub fn analyze_table(
        &mut self,
        namespace: tessari_types::NamespaceId,
        database: tessari_types::DatabaseId,
        table: tessari_types::TableId,
    ) -> Result<Vec<(IndexDefinition, IndexStatistics)>> {
        let mut catalog = Catalog::new(self);
        let records = catalog.record_count(table)?.unwrap_or(0);
        let indexes: Vec<IndexDefinition> = catalog
            .indexes_on(table)?
            .into_iter()
            .filter(|index| index.namespace == namespace && index.database == database)
            .filter(kept)
            .collect();
        let mut taken = Vec::with_capacity(indexes.len());
        for index in indexes {
            let statistics = self.analyze_index(&index, records)?;
            taken.push((index, statistics));
        }
        Ok(taken)
    }

    /// One index's statistic, when there is one and it still describes the
    /// index.
    ///
    /// `None` both when nothing was ever taken and when more has changed since
    /// than the statistic can absorb — the two cases a planner treats alike,
    /// by counting.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a held value cannot be
    /// decoded.
    pub fn fresh_statistics(&self, index: &IndexDefinition) -> Result<Option<IndexStatistics>> {
        if !kept(index) {
            return Ok(None);
        }
        let address = address_of(index);
        let Some(statistics) = held_statistics(self.store(), address)? else {
            return Ok(None);
        };
        let now = changes_of(self.store(), address)?;
        Ok(still_fresh(&statistics, now).then_some(statistics))
    }
}

impl Store {
    /// Take the statistics of indexes that have none or whose statistic has
    /// gone stale, at most [`STATISTICS_PER_PASS`] of them, on tables large
    /// enough for the planner to weigh an index against them.
    ///
    /// Answers how many were taken. This node's own work: every node keeps its
    /// own statistics, leader or not.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the catalog cannot be read.
    pub fn refresh_statistics(&self) -> Result<usize> {
        let transaction = self.begin()?;
        let mut stale = Vec::new();
        {
            let mut reading = self.begin()?;
            let mut catalog = Catalog::new(&mut reading);
            'tables: for namespace in catalog.namespaces()? {
                for database in catalog.databases_in(namespace.id)? {
                    for table in catalog.tables_in(namespace.id, database.id)? {
                        let Some(records) = catalog.record_count(table.id)? else {
                            continue;
                        };
                        if records < PLANNER_SCAN_FLOOR_RECORDS {
                            continue;
                        }
                        for index in catalog.indexes_on(table.id)? {
                            if index.namespace != namespace.id
                                || index.database != database.id
                                || !kept(&index)
                                || transaction.fresh_statistics(&index)?.is_some()
                            {
                                continue;
                            }
                            stale.push((index, records));
                            if stale.len() >= STATISTICS_PER_PASS {
                                break 'tables;
                            }
                        }
                    }
                }
            }
        }
        for (index, records) in &stale {
            transaction.analyze_index(index, *records)?;
        }
        Ok(stale.len())
    }
}

/// The running summary of one ordered walk over an index's entries.
struct Walk {
    arity: usize,
    entries: u64,
    distinct: Vec<u64>,
    previous: Vec<Option<Vec<u8>>>,
    run: Option<(Vec<u8>, u64)>,
    common: Vec<(Vec<u8>, u64)>,
    bounds: Vec<Vec<u8>>,
    step: u64,
    last_first: Option<Vec<u8>>,
}

impl Walk {
    fn new(arity: usize) -> Self {
        Self {
            arity,
            entries: 0,
            distinct: vec![0; arity],
            previous: vec![None; arity],
            run: None,
            common: Vec::new(),
            bounds: Vec::new(),
            step: 1,
            last_first: None,
        }
    }

    /// One entry, in index order.
    fn take(&mut self, values: &IndexValues) -> Result<()> {
        for (level, previous) in self.previous.iter_mut().enumerate() {
            let leading = values.leading_of(level.saturating_add(1))?;
            if previous.as_deref() != Some(leading) {
                if let Some(count) = self.distinct.get_mut(level) {
                    *count = count.saturating_add(1);
                }
                *previous = Some(leading.to_vec());
            }
        }
        let complete = values.leading_of(self.arity)?;
        match &mut self.run {
            Some((value, count)) if value.as_slice() == complete => {
                *count = count.saturating_add(1);
            }
            _ => {
                if let Some(ended) = self.run.take() {
                    self.keep_common(ended);
                }
                self.run = Some((complete.to_vec(), 1));
            }
        }
        // Equi-depth bounds: the first field's value every `step` entries. The
        // entry count is not known ahead, so the bounds are thinned to every
        // other one and the step doubled whenever there are twice too many —
        // which keeps them evenly spaced in entries without a second walk.
        let first = values.leading_of(1)?;
        if self.entries.is_multiple_of(self.step) {
            self.bounds.push(first.to_vec());
            if self.bounds.len() > STATISTICS_BUCKETS.saturating_mul(2) {
                self.bounds = self.bounds.iter().step_by(2).cloned().collect();
                self.step = self.step.saturating_mul(2);
            }
        }
        self.last_first = Some(first.to_vec());
        self.entries = self.entries.saturating_add(1);
        Ok(())
    }

    /// Keep a value among the most common, when it is one.
    fn keep_common(&mut self, (value, count): (Vec<u8>, u64)) {
        if count < 2 {
            return;
        }
        let at = self
            .common
            .iter()
            .position(|(_, held)| *held < count)
            .unwrap_or(self.common.len());
        if at < STATISTICS_COMMON_VALUES {
            self.common.insert(at, (value, count));
            self.common.truncate(STATISTICS_COMMON_VALUES);
        }
    }

    fn finish(mut self, records: u64, changes: u64) -> IndexStatistics {
        if let Some(ended) = self.run.take() {
            self.keep_common(ended);
        }
        if let Some(last) = self.last_first.take()
            && self.bounds.last() != Some(&last)
        {
            self.bounds.push(last);
        }
        IndexStatistics {
            records,
            changes,
            entries: self.entries,
            distinct: self.distinct,
            common: self.common,
            bounds: self.bounds,
        }
    }
}

/// How many entries hold exactly `values` — every field, or a leading run.
#[must_use]
pub fn estimate_equality(statistics: &IndexStatistics, values: &[Value]) -> u64 {
    let arity = statistics.distinct.len();
    let Some(first) = values.first() else {
        return statistics.entries;
    };
    // A first value outside every bound is outside every entry.
    let first = IndexValues::leading(std::slice::from_ref(first));
    if let (Some(lowest), Some(highest)) = (statistics.bounds.first(), statistics.bounds.last())
        && (first.as_slice() < lowest.as_slice() || first.as_slice() > highest.as_slice())
    {
        return 0;
    }
    if values.len() >= arity {
        let wanted = IndexValues::leading(values);
        if let Some((_, count)) = statistics.common.iter().find(|(value, _)| *value == wanted) {
            return *count;
        }
        let commonly: u64 = statistics.common.iter().map(|(_, count)| *count).sum();
        let others = statistics
            .distinct
            .last()
            .copied()
            .unwrap_or(1)
            .saturating_sub(u64::try_from(statistics.common.len()).unwrap_or(u64::MAX))
            .max(1);
        return statistics
            .entries
            .saturating_sub(commonly)
            .checked_div(others)
            .unwrap_or(0);
    }
    let distinct = values
        .len()
        .checked_sub(1)
        .and_then(|level| statistics.distinct.get(level))
        .copied()
        .unwrap_or(1)
        .max(1);
    statistics.entries.checked_div(distinct).unwrap_or(0)
}

/// How many entries hold a first value between two bounds, both included.
#[must_use]
pub fn estimate_range(
    statistics: &IndexStatistics,
    lower: Option<&Value>,
    upper: Option<&Value>,
) -> u64 {
    let lower = lower.map(|held| IndexValues::leading(std::slice::from_ref(held)));
    let upper = upper.map(|held| IndexValues::leading(std::slice::from_ref(held)));
    let buckets = statistics.bounds.len().saturating_sub(1);
    if buckets == 0 {
        return statistics.entries;
    }
    // Halves, so a bucket a bound cuts counts as half without a float.
    let mut halves = 0_u64;
    for pair in statistics.bounds.windows(2) {
        let [from, to] = pair else {
            continue;
        };
        let below = upper
            .as_ref()
            .is_some_and(|upper| from.as_slice() > upper.as_slice());
        let above = lower
            .as_ref()
            .is_some_and(|lower| to.as_slice() < lower.as_slice());
        if below || above {
            continue;
        }
        let whole = lower
            .as_ref()
            .is_none_or(|lower| lower.as_slice() <= from.as_slice())
            && upper
                .as_ref()
                .is_none_or(|upper| to.as_slice() <= upper.as_slice());
        halves = halves.saturating_add(if whole { 2 } else { 1 });
    }
    let buckets = u64::try_from(buckets).unwrap_or(u64::MAX);
    statistics
        .entries
        .saturating_mul(halves)
        .checked_div(buckets.saturating_mul(2))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
