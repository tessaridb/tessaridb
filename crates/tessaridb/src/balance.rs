//! Splitting and merging a table's shards without being asked (ADR-0113 D2).
//!
//! # Who acts
//!
//! The node that may commit to the store line: a table's shard map is a
//! catalog record, and two nodes each splitting the same hot shard would mint
//! two maps. Every other node does nothing here and applies the result from
//! the log, as it applies a split typed by an operator — because the act IS
//! that change, committed through the same catalog call and the same write
//! gate, so the shard registry learns it in the same critical section.
//!
//! # What it measures
//!
//! A shard's size is counted, not estimated: its live records walked a page at
//! a time through one snapshot, and the walk stops one past the bound, so a
//! pass costs each shard at most its bound and a page of memory. Its load is
//! the writes into it since the last pass, read off the log records written
//! since then — which name the shard each write was filed in, whether the shard
//! has a log of its own or shares its database's. A record count kept beside
//! each write was rejected: records are stamped with the shard they were
//! written into, so after a split the parent would keep counting what its
//! children now hold.
//!
//! # How much it does
//!
//! At most one act per table per pass — a split where a shard is too big or too
//! busy, otherwise a merge where two neighbours together are too small — so a
//! table that needs several converges over several passes and every step is
//! one record in the log.

use std::collections::BTreeMap;
use std::time::Instant;

use tessari_storage::{Catalog, LogId, ShardMap, ShardSpan, TableDefinition, Transaction, Window};
use tessari_types::{Reach, RecordId, Sequence, ShardId, TableId};

use crate::{Db, Result};

/// How many records one page of a shard's walk reads.
const PAGE: usize = 1_000;

/// How many log records one pass reads from one log at most: past it the
/// rate is at least what was read, which is all a bound needs.
const LOG_READ: usize = 100_000;

/// Where each log stood at the last pass, to count the writes since.
///
/// Owned by the caller's loop rather than by the node: one balancer runs per
/// process, and a measurement nobody else reads needs no lock.
#[derive(Debug, Default)]
pub struct ShardSamples {
    read_to: BTreeMap<LogId, Sequence>,
    at: Option<Instant>,
}

/// Writes a second into each shard since the last pass; `None` on the first.
type Rates = Option<BTreeMap<(TableId, ShardId), u64>>;

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Balanced {
    /// Shards split this pass.
    pub split: usize,
    /// Pairs of shards merged this pass.
    pub merged: usize,
    /// The last refusal met this pass, for the node to log.
    pub last_refusal: Option<String>,
}

/// One shard as the pass found it.
struct Measured {
    id: ShardId,
    /// Its live records, counted to one past the bound at most.
    records: usize,
    /// Whether the count reached the end of the shard.
    complete: bool,
    /// Writes a second since the last pass, once there was one.
    writes_per_second: Option<u64>,
}

impl Db {
    /// Split and merge the shards of every table that asked for it, when this
    /// node may commit to the store line.
    ///
    /// # Errors
    ///
    /// The store's failure to read the catalog, a shard or a log; a refused
    /// act is recorded in [`Balanced::last_refusal`] and the pass goes on.
    pub fn balance_shards(&self, samples: &mut ShardSamples) -> Result<Balanced> {
        let store = self.store();
        let mut balanced = Balanced::default();
        if !store.leads(Reach::Store)? {
            return Ok(balanced);
        }
        let tables = self.tables_balancing()?;
        let rates = self.writes_since(&tables, samples)?;
        for table in tables {
            let (Some(policy), Some(map)) = (table.auto_split, table.shards.as_ref()) else {
                continue;
            };
            let above = usize::try_from(policy.above).unwrap_or(usize::MAX);
            let measured = self.measure(&table, map, above, &rates)?;
            let busy = |shard: &Measured| {
                policy
                    .writes_per_second
                    .zip(shard.writes_per_second)
                    .is_some_and(|(bound, rate)| rate > bound)
            };
            let act = if let Some(shard) = measured
                .iter()
                .find(|shard| shard.records > above || busy(shard))
            {
                // The first id of the right half: half the bound into a shard
                // that is too big, half its records into one that is too busy.
                let into = if shard.records > above {
                    above / 2
                } else {
                    shard.records / 2
                };
                self.nth_record(&table, map, shard.id, into.max(1))?
                    .map(Act::Split)
            } else {
                let below = usize::try_from(policy.merge_below).unwrap_or(usize::MAX);
                measured
                    .windows(2)
                    .find(|pair| {
                        pair.iter().all(|shard| shard.complete)
                            && pair[0].records.saturating_add(pair[1].records) < below
                    })
                    .map(|pair| Act::Merge(pair[0].id, pair[1].id))
            };
            let Some(act) = act else {
                continue;
            };
            let mut writing = store.begin()?;
            let done = match &act {
                Act::Split(point) => Catalog::new(&mut writing)
                    .split_table(table.id, point)
                    .map(|_| ()),
                Act::Merge(first, second) => Catalog::new(&mut writing)
                    .merge_shards(table.id, *first, *second)
                    .map(|_| ()),
            };
            match done.and_then(|()| writing.commit().map(|_| ())) {
                Ok(()) => match act {
                    Act::Split(_) => balanced.split = balanced.split.saturating_add(1),
                    Act::Merge(..) => balanced.merged = balanced.merged.saturating_add(1),
                },
                Err(why) => balanced.last_refusal = Some(format!("table `{}`: {why}", table.name)),
            }
        }
        Ok(balanced)
    }

    /// Every table that asked to be balanced and is split.
    fn tables_balancing(&self) -> Result<Vec<TableDefinition>> {
        let mut reading = self.store().begin()?;
        let catalog = Catalog::new(&mut reading);
        let mut found = Vec::new();
        for namespace in catalog.namespaces()? {
            for database in catalog.databases_in(namespace.id)? {
                found.extend(
                    catalog
                        .tables_in(namespace.id, database.id)?
                        .into_iter()
                        .filter(|table| table.auto_split.is_some() && table.shards.is_some()),
                );
            }
        }
        reading.rollback();
        Ok(found)
    }

    /// Writes a second into each shard of `tables` since the last pass: every
    /// log a write into one of them can land in, read from where the last pass
    /// left it. The first pass only learns where each log stands.
    fn writes_since(
        &self,
        tables: &[TableDefinition],
        samples: &mut ShardSamples,
    ) -> Result<Rates> {
        let store = self.store();
        let now = Instant::now();
        let mut counted: BTreeMap<(TableId, ShardId), u64> = BTreeMap::new();
        for log in store.logs()? {
            let holds = |table: &TableDefinition| {
                log.home
                    .contains(Reach::Database(table.namespace, table.database))
                    || matches!(log.home, Reach::Shard(_, _, id, _) if id == table.id)
            };
            if !tables.iter().any(holds) {
                continue;
            }
            let tail = store.committed_tail(log)?;
            // A log first met after the first pass — a shard's own log is
            // opened by the first write into it — holds only writes since.
            let next = match samples.read_to.insert(log, tail) {
                Some(from) => Sequence::new(from.get().saturating_add(1)),
                None if samples.at.is_some() => store.log_start(log)?,
                None => continue,
            };
            if next > tail {
                continue;
            }
            // A log pruned past where the last pass left it has lost what it
            // would have counted; the pass counts nothing from it this time.
            let Ok(records) = store.log_records(log, next, LOG_READ) else {
                continue;
            };
            for (_, record) in records {
                for mutation in record.mutations() {
                    if let Some(shard) = mutation.shard {
                        let held = counted.entry((mutation.table, shard)).or_insert(0);
                        *held = held.saturating_add(1);
                    }
                }
            }
        }
        let Some(then) = samples.at.replace(now) else {
            return Ok(None);
        };
        let elapsed = now.saturating_duration_since(then).as_millis().max(1);
        Ok(Some(
            counted
                .into_iter()
                .map(|(shard, writes)| {
                    let rate = u128::from(writes)
                        .saturating_mul(1_000)
                        .checked_div(elapsed)
                        .unwrap_or(u128::MAX);
                    (shard, u64::try_from(rate).unwrap_or(u64::MAX))
                })
                .collect(),
        ))
    }

    /// Each live shard of `table`, counted to one past `above`, with its load.
    fn measure(
        &self,
        table: &TableDefinition,
        map: &ShardMap,
        above: usize,
        rates: &Rates,
    ) -> Result<Vec<Measured>> {
        let reading = self.store().begin()?;
        let mut measured = Vec::new();
        for span in map.spans() {
            let (records, complete) = count(&reading, table, &span, above.saturating_add(1))?;
            measured.push(Measured {
                id: span.id,
                records,
                complete,
                writes_per_second: rates
                    .as_ref()
                    .map(|rates| rates.get(&(table.id, span.id)).copied().unwrap_or(0)),
            });
        }
        reading.rollback();
        Ok(measured)
    }

    /// The `n`th live record of shard `id` (from zero), if it has that many.
    fn nth_record(
        &self,
        table: &TableDefinition,
        map: &ShardMap,
        id: ShardId,
        n: usize,
    ) -> Result<Option<RecordId>> {
        let Some(span) = map.spans().find(|span| span.id == id) else {
            return Ok(None);
        };
        let reading = self.store().begin()?;
        let mut seen = 0_usize;
        let mut after: Option<RecordId> = None;
        let found = loop {
            let page = page(&reading, table, &span, after.as_ref())?;
            let Some((last, _)) = page.last() else {
                break None;
            };
            let left = n.saturating_sub(seen);
            if let Some((point, _)) = page.get(left) {
                break Some(point.clone());
            }
            seen = seen.saturating_add(page.len());
            after = Some(last.clone());
        };
        reading.rollback();
        Ok(found)
    }
}

enum Act {
    Split(RecordId),
    Merge(ShardId, ShardId),
}

/// The live records of a shard, counted to `limit`, and whether the count
/// reached its end.
fn count(
    reading: &Transaction<'_>,
    table: &TableDefinition,
    span: &ShardSpan<'_>,
    limit: usize,
) -> Result<(usize, bool)> {
    let mut counted = 0_usize;
    let mut after: Option<RecordId> = None;
    loop {
        let found = page(reading, table, span, after.as_ref())?;
        counted = counted.saturating_add(found.len());
        let Some((last, _)) = found.last() else {
            return Ok((counted, true));
        };
        if found.len() < PAGE {
            return Ok((counted, true));
        }
        if counted >= limit {
            return Ok((counted, false));
        }
        after = Some(last.clone());
    }
}

/// One page of a shard's live records, after `after`.
fn page(
    reading: &Transaction<'_>,
    table: &TableDefinition,
    span: &ShardSpan<'_>,
    after: Option<&RecordId>,
) -> Result<Vec<(RecordId, Vec<u8>)>> {
    Ok(reading.records_between(
        table.namespace,
        table.database,
        table.id,
        Window {
            from: span.from,
            to: span.to.map(|to| (to, false)),
        },
        after,
        PAGE,
    )?)
}

#[cfg(test)]
mod tests;
