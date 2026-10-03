//! What the balancing pass last measured of each table's shards (ADR-0113 D4).
//!
//! Published by the one pass that measures — the store line's leader's — and
//! read by `INFO FOR TABLE` and `/metrics`, so a scrape reads what was counted
//! and never walks a shard itself (a walk per scrape grows with the table).
//! Held in memory and never persisted: a measurement that outlived the process
//! that took it would be a claim nobody can check.

use dashmap::DashMap;
use tessari_types::{ShardId, TableId};

/// One shard as the last pass found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledShard {
    /// Which shard.
    pub shard: ShardId,
    /// Its live records, counted to one past the table's bound at most.
    pub records: u64,
    /// Whether the count reached the end of the shard: `false` means at least
    /// `records`.
    pub complete: bool,
    /// Writes a second into it since the pass before, once there was one.
    pub writes_per_second: Option<u64>,
}

/// One table's shards as the last pass found them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledTable {
    /// The table as an operator names it, `namespace.database.table`.
    pub name: String,
    /// Each live shard, in map order.
    pub shards: Vec<SampledShard>,
    /// What the balancer last did to the table, if anything since this
    /// process started.
    pub last_act: Option<String>,
}

/// The samples, one entry per balanced table, each replaced whole per pass.
#[derive(Debug, Default)]
pub(crate) struct SampledShards(DashMap<TableId, SampledTable>);

impl SampledShards {
    /// Replace the samples with `tables`: a table no longer balanced drops
    /// out, and a table with no act this pass keeps the last one it had.
    pub(crate) fn publish(&self, tables: Vec<(TableId, SampledTable)>) {
        self.0
            .retain(|held, _| tables.iter().any(|(table, _)| table == held));
        for (table, mut sampled) in tables {
            // Read-modify-write through the entry, never a guard held across a
            // second call on the map.
            self.0
                .entry(table)
                .and_modify(|held| {
                    if sampled.last_act.is_none() {
                        sampled.last_act = held.last_act.take();
                    }
                    *held = sampled.clone();
                })
                .or_insert(sampled);
        }
    }

    /// Every table's sample, in table order.
    pub(crate) fn all(&self) -> Vec<(TableId, SampledTable)> {
        let mut found: Vec<_> = self
            .0
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        found.sort_by_key(|(table, _)| *table);
        found
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{SampledShard, SampledShards, SampledTable};
    use tessari_types::{ShardId, TableId};

    fn table(last_act: Option<&str>) -> SampledTable {
        SampledTable {
            name: "prod.shop.orders".to_owned(),
            shards: vec![SampledShard {
                shard: ShardId::new(1),
                records: 3,
                complete: true,
                writes_per_second: None,
            }],
            last_act: last_act.map(str::to_owned),
        }
    }

    #[test]
    fn a_pass_replaces_the_samples_and_keeps_the_last_act_it_did_not_replace() {
        let held = SampledShards::default();
        held.publish(vec![
            (TableId::new(1), table(Some("split at 'g'"))),
            (TableId::new(2), table(None)),
        ]);
        held.publish(vec![(TableId::new(1), table(None))]);
        let all = held.all();
        assert_eq!(all.len(), 1, "a table no longer balanced dropped out");
        assert_eq!(all[0].1.last_act.as_deref(), Some("split at 'g'"));
        held.publish(vec![(TableId::new(1), table(Some("merged 2 and 3")))]);
        assert_eq!(held.all()[0].1.last_act.as_deref(), Some("merged 2 and 3"));
    }
}
