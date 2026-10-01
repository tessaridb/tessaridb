//! `ALTER TABLE … SPLIT AT` and `… MERGE SHARD` in the catalog (ADR-0095).
//!
//! Only the definition is written. The map reaches the per-process registry
//! when the commit carrying it lands, under the write gate, on the leader and on
//! every follower that applies it — never here, before anything has committed.

use tessari_types::{RecordId, ShardId, TableId};

use super::shard::Unmovable;
use super::{Catalog, ShardMap, system};
use crate::error::{Error, Result};

impl Catalog<'_, '_> {
    /// Split the shard of table `id` that holds `point`, at `point`.
    ///
    /// # Errors
    ///
    /// [`Error::NotASplitTable`] for a table declared without `SPLIT AT`,
    /// [`Error::SplitPointOnABoundary`] for a point that already begins a shard,
    /// [`Error::ShardNamedByAReplica`] when a replica row names the shard, and
    /// an error when the catalog cannot be read.
    pub fn split_table(&mut self, id: TableId, point: &RecordId) -> Result<ShardMap> {
        self.move_shards(id, |name, map| {
            let holder = map.shard_of(point);
            Ok((vec![holder], refused(name, map.split_at(point))?))
        })
    }

    /// Merge the adjacent live shards `first` and `second` of table `id`.
    ///
    /// # Errors
    ///
    /// [`Error::NotASplitTable`], [`Error::ShardNotLive`] for a shard that is
    /// unknown or retired, [`Error::ShardsNotAdjacent`],
    /// [`Error::ShardNamedByAReplica`], and an error when the catalog cannot be
    /// read.
    pub fn merge_shards(
        &mut self,
        id: TableId,
        first: ShardId,
        second: ShardId,
    ) -> Result<ShardMap> {
        self.move_shards(id, |name, map| {
            Ok((
                vec![first, second],
                refused(name, map.merged(first, second))?,
            ))
        })
    }

    /// Change table `id`'s map by `change`, which answers the shards it retires
    /// and the new map; refuse it when a replica row names one of those shards.
    fn move_shards(
        &mut self,
        id: TableId,
        change: impl FnOnce(&str, &ShardMap) -> Result<(Vec<ShardId>, ShardMap)>,
    ) -> Result<ShardMap> {
        let Some(mut definition) = self.table(id)? else {
            return Err(Error::NoSuchParent {
                entity: "table",
                id: id.get(),
            });
        };
        let Some(map) = definition.shards.as_ref() else {
            return Err(Error::NotASplitTable {
                table: definition.name,
            });
        };
        let (retiring, moved) = change(&definition.name, map)?;
        // A row holds one reach, so it cannot be carried to two successors; it
        // would go on naming a shard nothing writes again (ADR-0095 amendment).
        for replica in self.replicas()? {
            for reach in [replica.replicates, replica.leads].into_iter().flatten() {
                if let crate::Reach::Shard(namespace, database, table, shard) = reach
                    && (namespace, database, table)
                        == (definition.namespace, definition.database, id)
                    && retiring.contains(&shard)
                {
                    return Err(Error::ShardNamedByAReplica {
                        table: definition.name,
                        shard: shard.get(),
                        replica: replica.name,
                    });
                }
            }
        }
        definition.shards = Some(moved.clone());
        self.write(system::TABLES, id.get(), &definition.to_value());
        Ok(moved)
    }
}

/// A map change refused in the words of the table it was asked of.
fn refused(table: &str, moved: std::result::Result<ShardMap, Unmovable>) -> Result<ShardMap> {
    moved.map_err(|why| match why {
        Unmovable::OnABoundary => Error::SplitPointOnABoundary {
            table: table.to_owned(),
        },
        Unmovable::NotLive(shard) => Error::ShardNotLive {
            table: table.to_owned(),
            shard: shard.get(),
        },
        Unmovable::NotAdjacent => Error::ShardsNotAdjacent {
            table: table.to_owned(),
        },
        Unmovable::Exhausted => Error::IdSpaceExhausted { level: "shard" },
    })
}
