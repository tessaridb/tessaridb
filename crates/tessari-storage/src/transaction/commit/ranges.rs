//! Which ranges a commit writes, and whether each may be written now.

use super::{Placement, Transaction, is_a_leadership};
use crate::catalog::{Reach, ShardMap};
use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tessari_encoding::{RecordValue, decode_payload};
use tessari_types::TableId;

impl Transaction<'_> {
    /// The ranges this transaction's writes address, deduplicated.
    ///
    /// A [`Reach::Database`] per write, because that is the narrowest range a
    /// record belongs to and [`Reach::contains`] widens it: a leadership over
    /// the namespace or over the whole store covers these without the gate
    /// having to construct those ranges itself.
    ///
    /// # A leadership row is judged by the range it describes
    ///
    /// One exception, and it is the wall Q-597 named before anything could reach
    /// it. A leadership row lives in the system tenancy, so by address it is a
    /// write into `Reach::Database(0, 0)` — and in a cluster where somebody else
    /// holds `Reach::Store`, that range is led elsewhere. A node that has just
    /// won a round for `Namespace(Y)` would therefore be refused permission to
    /// record the leadership a majority granted it: the lease is installed and
    /// cannot fail, but the log never learns about it, so every other node goes
    /// on routing `Namespace(Y)`'s writes to the store-wide leader.
    ///
    /// The row is a **claim about `Namespace(Y)`**, so the question worth asking
    /// is who leads `Namespace(Y)` — which is this node, by construction, because
    /// the row exists only because it won that round. Asking instead who leads
    /// the tenancy the row happens to be stored in is asking about the filing
    /// cabinet rather than the document.
    ///
    /// This is narrower than exempting the system tenancy, which was the other
    /// way out and would have let any node holding any lease write any system
    /// row — another node's membership included.
    ///
    /// A row that cannot be decoded is **refused** rather than judged by its
    /// address: a leadership whose range this node cannot read is one it cannot
    /// place, and placing it wrongly is the failure this whole function exists to
    /// prevent.
    /// Whether **every** range this transaction writes was declared multi-master.
    ///
    /// All of them and not any of them. A transaction that writes a declared
    /// range and an undeclared one is still a write into a range that has a
    /// single leader, and exempting it because one of its ranges was declared
    /// would let the undeclared write travel under the declared one's cover.
    /// Whether `home` admits two writers, read through this transaction's own
    /// catalog rather than a new one — the commit path asks it on every write
    /// under a leadership (ADR-0107), and a second transaction per commit is a
    /// round trip a leader would pay on each one.
    pub(crate) fn admits_two_writers_here(&mut self, home: Reach) -> Result<bool> {
        let (Some(namespace), _) = home.parts() else {
            return Ok(false);
        };
        Ok(crate::catalog::Catalog::new(self)
            .namespace(namespace)?
            .and_then(|definition| definition.class)
            .is_some_and(tessari_types::ReplicationClass::admits_two_writers))
    }

    pub(crate) fn every_range_admits_two_writers(&self, ranges: &BTreeSet<Reach>) -> Result<bool> {
        for range in ranges {
            if !self.store.admits_two_writers(*range)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Refuse a placed range this node may not write on its own line, and
    /// answer the ranges left to the store line (ADR-0082).
    ///
    /// A live line admits; a spent one is `LeaseSpent` for that range alone; no
    /// line at all is `NoLeadershipYet` in a cluster — unless the range admits
    /// two writers, by the same predicate the store line's question consults.
    /// The store leader is deliberately NOT a fallback for a placed range with
    /// no live leader: the placement carved it out, and writing it anyway is
    /// the two-writer window the carving exists to close.
    pub(crate) fn admitted_on_their_lines(
        &self,
        ranges: &BTreeSet<Reach>,
        placed: &BTreeSet<Reach>,
        me: &[u8; tessari_encoding::NODE_ID_LEN],
    ) -> Result<BTreeSet<Reach>> {
        let mut on_the_store = BTreeSet::new();
        for range in ranges {
            let line = crate::catalog::governing(placed, *range);
            if line == Reach::Store {
                on_the_store.insert(*range);
                continue;
            }
            match self.store.line_standing(line) {
                crate::lines::Standing::Live => {}
                crate::lines::Standing::Spent(for_the_last) => {
                    return Err(Error::LeaseSpent { for_the_last });
                }
                crate::lines::Standing::NotHeld => {
                    if self.store.in_a_cluster(me)? && !self.store.admits_two_writers(*range)? {
                        return Err(Error::NoLeadershipYet);
                    }
                }
            }
        }
        Ok(on_the_store)
    }

    /// The fences that admitted this commit, judged again once it holds its turn.
    ///
    /// Admission runs before the gate and the wait for the gate is unbounded — a
    /// queue of writers each paying a device sync, or one sync that stalls — while
    /// the fence's guard covers only the drift between two clocks. Judged once, a
    /// commit admitted with a millisecond left would land after the fence shut,
    /// when another node may already have been granted the leadership. Only the
    /// in-memory fences are asked again: they are what time changes.
    pub(crate) fn refuse_if_fenced_since(
        &self,
        store_line: bool,
        placed: &BTreeSet<Reach>,
        ranges: &BTreeSet<Reach>,
    ) -> Result<()> {
        if store_line && let Some(for_the_last) = self.store.lease_spent() {
            return Err(Error::LeaseSpent { for_the_last });
        }
        for range in ranges {
            let line = crate::catalog::governing(placed, *range);
            if line == Reach::Store {
                continue;
            }
            if let crate::lines::Standing::Spent(for_the_last) = self.store.line_standing(line) {
                return Err(Error::LeaseSpent { for_the_last });
            }
        }
        Ok(())
    }

    pub(crate) fn ranges_written(&self, placement: &Placement) -> Result<BTreeSet<Reach>> {
        let mut ranges: BTreeSet<Reach> = self.records_ranges(placement)?;
        // A decision across leaders writes no records, and is still a write into
        // its coordinator's range: it is admitted, fenced and refused there
        // exactly as a record written into that range would be (ADR-0112 D4).
        if let Some(coordinator) = self.decision_range() {
            ranges.insert(coordinator);
        }
        Ok(ranges)
    }

    /// The ranges the buffered records fall in.
    fn records_ranges(&self, placement: &Placement) -> Result<BTreeSet<Reach>> {
        self.writes
            .iter()
            .map(|(address, value)| match value {
                RecordValue::Present(payload) if is_a_leadership(address) => {
                    let described = crate::catalog::LeadershipDefinition::from_value(
                        &decode_payload(payload)?,
                    )?;
                    Ok(described.range)
                }
                _ => Ok(match placement.shard_of(address) {
                    Some(shard) => {
                        Reach::Shard(address.namespace, address.database, address.table, shard)
                    }
                    None => Reach::Database(address.namespace, address.database),
                }),
            })
            .collect()
    }

    /// The shard maps of every split table this transaction writes (G031).
    ///
    /// Resolved once and asked twice — by the admission gate, for the ranges it
    /// judges, and by the log record, for the shard it stamps on each mutation —
    /// because two lookups of one fact are two answers that can disagree, and a
    /// record admitted under one shard and filed under another is exactly the
    /// failure the stamp exists to make impossible.
    ///
    /// The registry answers almost always; a miss reads the committed catalog
    /// once and teaches the registry, which is what a node that became leader
    /// after the tables were declared elsewhere meets on its first write.
    pub(crate) fn placement(&self) -> Result<Placement> {
        let mut maps: BTreeMap<TableId, Arc<ShardMap>> = BTreeMap::new();
        let mut unread: BTreeSet<TableId> = BTreeSet::new();
        for address in self.writes.keys() {
            if address.namespace == crate::catalog::system::SYSTEM_NAMESPACE
                || maps.contains_key(&address.table)
                || unread.contains(&address.table)
            {
                continue;
            }
            match self.store.shards().known(address.table) {
                Some(Some(map)) => {
                    maps.insert(address.table, map);
                }
                Some(None) => {}
                None => {
                    unread.insert(address.table);
                }
            }
        }
        if !unread.is_empty() {
            let mut view = self.store.begin()?;
            let catalog = crate::catalog::Catalog::new(&mut view);
            for table in unread {
                // A record naming a table the catalog does not hold is not split
                // by anything this store knows of, and is filed as it always was;
                // whether such a write is allowed at all is not this function's
                // question. It is NOT learned, so a table declared later is read
                // again rather than remembered as unsplit.
                let Some(definition) = catalog.table(table)? else {
                    continue;
                };
                self.store.shards().learn(table, definition.shards.as_ref());
                if let Some(map) = definition.shards {
                    maps.insert(table, Arc::new(map));
                }
            }
        }
        Ok(Placement { maps })
    }
}
