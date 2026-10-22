//! Reading a store's state out, and writing one into an empty store (ADR-0091).
//!
//! The reading is [`crate::state`]'s; the entry point is here, beside the
//! writing, so the one list of what reaches past the session to the store holds
//! both halves.
//!
//! A chunk is written the way an applied log record is: validated against the
//! catalog, given the next version of this store, and derived by the same
//! `maintain` functions in the same order — so indexes, adjacency, counts,
//! expiry, eviction and topic entries are rebuilt rather than carried. What an
//! apply also does and this does not is file the record in a log and move that
//! log's position: a restored state has no log below the positions it came
//! from, and those are written once, at the end, by [`Store::finish_state`].
//!
//! A topic's entries are the exception, carried rather than derived: a restore
//! meets messages in key order and would number them in that order, where the
//! source numbered them as they were committed. So the entries a chunk derived
//! are cleared and the source's are installed in their place, by
//! [`Store::restore_topic_entries`], before the state is finished.
//!
//! The result is the source store as it would stand pruned at those positions,
//! which is a state this engine already has — so a log backup taken from the
//! source `FROM` the next position applies onto it.

use std::collections::{HashMap, HashSet};

use tessari_encoding::{
    AppliedPositionKey, ExpiryMark, LogId, LogRecord, LogStartKey, Mutation, ReclaimFloorKey,
    RecordKey, RecordValue, StampedValue, StoreKey, StoreValue, TopicEntryKey, TopicHeadKey,
    TopicOffsetKey,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence, TableId};

use super::Store;
use crate::error::Result;
use crate::state::{TopicEntry, TopicHead};
use crate::transaction::RecordAddress;

type Topic = (NamespaceId, DatabaseId, TableId);

/// How many stored keys one page of a topic's record walk reads, and how many
/// messages one removal chunk carries.
const PAGE: usize = 1_000;

/// What installing a state's topic entries has done so far, carried from one
/// batch of entries to the next.
#[derive(Debug, Default)]
pub struct RestoredTopics {
    /// The topics whose derived entries have been cleared.
    cleared: HashSet<Topic>,
    /// Per topic, the lowest position carried for a message this store does
    /// not hold — one appended after the state's version.
    unheld: HashMap<Topic, u64>,
}

impl RestoredTopics {
    /// `heads`, each lowered to where its topic stood at the state's version.
    ///
    /// The heads are read after the state, so a message appended meanwhile has
    /// raised one. Positions are given in commit order, so every position at or
    /// above the lowest one a message outside the state holds was given after
    /// the state was read — and the log above the state gives them again.
    #[must_use]
    pub fn heads(&self, heads: Vec<TopicHead>) -> Vec<TopicHead> {
        heads
            .into_iter()
            .map(|mut head| {
                if let Some(first) = self
                    .unheld
                    .get(&(head.namespace, head.database, head.table))
                {
                    head.last = head.last.min(first.saturating_sub(1));
                }
                head
            })
            .collect()
    }
}

impl Store {
    /// Begin reading this store's current state.
    ///
    /// # Errors
    ///
    /// Returns an error when a position or the version cannot be read.
    pub async fn read_state(&self) -> Result<crate::state::StateReader<'_>> {
        crate::state::StateReader::open(self, tessari_types::Reach::Store).await
    }

    /// Begin reading the part of this store a peer subscribed at `within` is
    /// given: its records by the rule a collect applies, the logs inside or
    /// above it, and the topics inside it.
    ///
    /// # Errors
    ///
    /// Returns an error when a position or the version cannot be read.
    pub async fn read_state_within(
        &self,
        within: tessari_types::Reach,
    ) -> Result<crate::state::StateReader<'_>> {
        crate::state::StateReader::open(self, within).await
    }

    /// Whether nothing has ever been written into this store.
    ///
    /// # Errors
    ///
    /// Returns an error when the version or the log list cannot be read.
    pub async fn holds_nothing(&self) -> Result<bool> {
        Ok(self.committed_version().await? == Sequence::ZERO && self.logs().await?.is_empty())
    }

    /// Write one chunk of a restored state.
    ///
    /// # Errors
    ///
    /// Returns the store's refusal when a record does not satisfy the catalog
    /// already restored, and a backend error when the batch cannot land.
    pub async fn restore_state_chunk(&self, record: &LogRecord) -> Result<()> {
        let _turn = self.writing.hold().await;
        self.writing.land_all(self.backend.as_ref()).await;
        crate::schema::validate(self, record).await?;
        let version = Sequence::new(self.committed_version().await?.get().saturating_add(1));
        let landed = self
            .derive_and_land(
                record,
                crate::log::state_batch(version, record),
                version,
                // A state lands in no log: what it carries is applied, not logged.
                None,
                crate::gate::Landing::Synced,
            )
            .await;
        // A state writes a decided record as `PENDING` and then its decision
        // (ADR-0112 D9a). Copied onto a store already holding the decision —
        // a follower copying its leader's state again — the first is refused
        // and the second lands as it stands; any other refusal is a real one.
        match (landed, record.part_of()) {
            (
                Err(crate::error::Error::AcrossDecided { .. }),
                Some(tessari_encoding::Across {
                    part: tessari_encoding::Part::Decide(decided),
                    ..
                }),
            ) if decided.decision == tessari_encoding::Decision::Pending => Ok(()),
            (landed, _) => landed,
        }
    }

    /// Install a state's topic entries in place of the ones its chunks derived.
    ///
    /// The first time a topic is met, every entry this store holds for it is
    /// cleared; then each carried entry is written, with its identity entry
    /// beside it, for a message this store holds. One for a message it does not
    /// hold was appended after the state's version and is passed over —
    /// recorded in `restored`, so [`RestoredTopics::heads`] can lower the
    /// topic's head to match.
    ///
    /// # Errors
    ///
    /// Returns a backend error when a record cannot be read or the batch cannot
    /// land.
    pub async fn restore_topic_entries(
        &self,
        entries: &[TopicEntry],
        restored: &mut RestoredTopics,
    ) -> Result<()> {
        let _turn = self.writing.hold().await;
        self.writing.land_all(self.backend.as_ref()).await;
        let view = self.begin_local().await?;
        let mut batch = WriteBatch::new();
        for entry in entries {
            let key = &entry.key;
            let topic = (key.namespace, key.database, key.table);
            if restored.cleared.insert(topic) {
                let (namespace, database, table) = topic;
                self.backend
                    .delete_range(
                        TopicOffsetKey::keyspace(),
                        &KeyRange::prefix(&TopicOffsetKey::table_prefix(
                            namespace, database, table,
                        )),
                    )
                    .await?;
                self.backend
                    .delete_range(
                        TopicEntryKey::keyspace(),
                        &KeyRange::prefix(&TopicEntryKey::table_prefix(namespace, database, table)),
                    )
                    .await?;
            }
            let address =
                RecordAddress::new(key.namespace, key.database, key.table, key.id.clone());
            let held = view
                .read_newest_stamped(&address)
                .await?
                .is_some_and(|(_, stamped)| !stamped.value().is_tombstone());
            if !held {
                let lowest = restored.unheld.entry(topic).or_insert(key.offset);
                *lowest = (*lowest).min(key.offset);
                continue;
            }
            batch = batch
                .put(
                    TopicOffsetKey::keyspace(),
                    key.encode(),
                    entry.mark.encode(),
                )
                .put(
                    TopicEntryKey::keyspace(),
                    TopicEntryKey {
                        namespace: key.namespace,
                        database: key.database,
                        table: key.table,
                        id: key.id.clone(),
                        offset: key.offset,
                    }
                    .encode(),
                    ExpiryMark.encode(),
                );
        }
        drop(view);
        self.writing
            .apply(batch, self.backend.as_ref(), crate::gate::Landing::Synced)
            .await?;
        Ok(())
    }

    /// Finish the topics of a state that carried their entries, inside
    /// `within`, and answer how many messages were removed.
    ///
    /// Two things the entries cannot do on their own. A topic none of whose
    /// entries were carried still holds the ones its chunks derived, and they
    /// are cleared. And a message the state holds with no entry was removed
    /// from the source — by retention or a trim, always from the front — after
    /// the state's version and before its entries were read: no position can
    /// be given to it that the source gave, and the log above the state removes
    /// it anyway, so it is removed here. A topic's copy is then the messages the
    /// state holds that the source still held when its entries were read.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or a removal is refused.
    pub async fn finish_topics(&self, within: Reach, restored: &RestoredTopics) -> Result<u64> {
        let request = ScanRequest {
            keyspace: TopicHeadKey::keyspace(),
            range: KeyRange::prefix(&[TopicHeadKey::KIND.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut removed = 0_u64;
        for (key, _) in self.backend.scan(&request).await? {
            let head = TopicHeadKey::decode(key.as_slice())?;
            if !within.contains(Reach::Database(head.namespace, head.database)) {
                continue;
            }
            let topic = (head.namespace, head.database, head.table);
            if !restored.cleared.contains(&topic) {
                let _turn = self.writing.hold().await;
                self.writing.land_all(self.backend.as_ref()).await;
                self.clear_topic(topic).await?;
            }
            removed = removed.saturating_add(self.remove_unplaced(topic).await?);
        }
        Ok(removed)
    }

    /// Remove both of a topic's entry indexes.
    async fn clear_topic(&self, (namespace, database, table): Topic) -> Result<()> {
        self.backend
            .delete_range(
                TopicOffsetKey::keyspace(),
                &KeyRange::prefix(&TopicOffsetKey::table_prefix(namespace, database, table)),
            )
            .await?;
        self.backend
            .delete_range(
                TopicEntryKey::keyspace(),
                &KeyRange::prefix(&TopicEntryKey::table_prefix(namespace, database, table)),
            )
            .await?;
        Ok(())
    }

    /// Remove every message of `topic` that holds no position.
    async fn remove_unplaced(&self, topic: Topic) -> Result<u64> {
        let (namespace, database, table) = topic;
        let every = KeyRange::prefix(&RecordKey::table_prefix(namespace, database, table));
        let mut unplaced: Vec<Mutation> = Vec::new();
        let mut removed = 0_u64;
        let mut after = None;
        let mut decided: Option<Vec<u8>> = None;
        loop {
            let request = ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: after
                    .as_ref()
                    .map_or_else(|| every.clone(), |after| every.resuming_after(after)),
                direction: ScanDirection::Forward,
                limit: Some(PAGE),
            };
            let page = self.backend.sweep(&request).await?;
            let last = page.len() < PAGE;
            for (key, value) in page {
                after = Some(key.clone());
                let stored = RecordKey::decode(key.as_slice())?;
                let prefix = RecordKey::versions_prefix(namespace, database, table, &stored.id);
                // Versions sort newest-first: the first one decides the record.
                if decided.as_deref() == Some(prefix.as_slice()) {
                    continue;
                }
                decided = Some(prefix);
                if StampedValue::decode(value.as_slice())?
                    .value()
                    .is_tombstone()
                    || crate::topic::position_of(self, topic, &stored.id)
                        .await?
                        .is_some()
                {
                    continue;
                }
                unplaced.push(Mutation {
                    namespace,
                    database,
                    table,
                    id: stored.id,
                    shard: None,
                    value: StampedValue::new(RecordValue::Tombstone),
                });
            }
            if unplaced.len() >= PAGE || (last && !unplaced.is_empty()) {
                removed = removed.saturating_add(u64::try_from(unplaced.len()).unwrap_or(u64::MAX));
                self.restore_state_chunk(&LogRecord::new(std::mem::take(&mut unplaced)))
                    .await?;
            }
            if last {
                return Ok(removed);
            }
        }
    }

    /// Finish a restored state: where each log stood, where its history
    /// starts, and how far each topic had counted.
    ///
    /// Each log stands at the position the state was read at and starts one
    /// past it, so a reader asking for anything below is refused rather than
    /// told the log is level. History below the restore's own last version is
    /// reclaimed by definition — none of it is the source's history — so the
    /// floor is set there and a historical read below it is refused. A topic's
    /// head is raised to the source's and never lowered, because the messages
    /// restored may be fewer than the topic had given.
    ///
    /// # Errors
    ///
    /// Returns a backend error when the batch cannot land.
    pub async fn finish_state(
        &self,
        positions: &[(LogId, Sequence)],
        topics: &[TopicHead],
    ) -> Result<()> {
        let _turn = self.writing.hold().await;
        self.writing.land_all(self.backend.as_ref()).await;
        let mut batch = WriteBatch::new();
        for (log, at) in positions {
            batch = batch
                .put(
                    AppliedPositionKey::keyspace(),
                    AppliedPositionKey::new(*log).encode(),
                    at.encode(),
                )
                .put(
                    LogStartKey::keyspace(),
                    LogStartKey::new(*log).encode(),
                    Sequence::new(at.get().saturating_add(1)).encode(),
                );
        }
        for topic in topics {
            let key = TopicHeadKey {
                namespace: topic.namespace,
                database: topic.database,
                table: topic.table,
            };
            let held =
                crate::topic::head(self, (topic.namespace, topic.database, topic.table)).await?;
            if topic.last > held {
                batch = batch.put(
                    TopicHeadKey::keyspace(),
                    key.encode(),
                    Sequence::new(topic.last).encode(),
                );
            }
        }
        batch = batch.put(
            ReclaimFloorKey::keyspace(),
            ReclaimFloorKey.encode(),
            self.committed_version().await?.encode(),
        );
        self.writing
            .apply(batch, self.backend.as_ref(), crate::gate::Landing::Synced)
            .await?;
        Ok(())
    }
}
