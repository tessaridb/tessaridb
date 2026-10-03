//! A store's current state, read out as records and written back in (ADR-0091).
//!
//! # Why a second way out when the log already is the store
//!
//! The log is every commit ever made, so a backup of it grows with history
//! rather than with what the store holds: one record written ten thousand times
//! is ten thousand log records. And once a log is pruned it cannot be backed up
//! at all, because the part that explained the oldest state is gone. What
//! survives both is the state itself — the newest version of every record at one
//! version of the store.
//!
//! # What is read, and what is left to be derived again
//!
//! Every record in every tenancy, the catalog's included, since the catalog is
//! records (ADR-0009). Not read: anything a `maintain` function writes, because a
//! restore runs those same functions and a carried copy would count twice — which
//! is why [`RECORD_COUNTS`](crate::catalog::system::RECORD_COUNTS) is skipped
//! here. Read separately: a topic's last-given position, which is written by
//! `topic::maintain` but never removed, so the messages that survive retention
//! cannot say how far the topic had counted.
//!
//! # One version, and where each log stood at it
//!
//! A record's version is the store-wide counter, not a log position
//! (`crate::log::apply_batch`), so one read at one version is consistent across
//! every log. The reader holds a registered transaction for its whole life, so
//! reclamation keeps what it has yet to read. The applied position of every log
//! is read with the version on both sides of it: a commit writes the two in one
//! batch, so a version unchanged across the reads means no position moved.
//!
//! The reader belongs to one caller on one thread; the shard maps it learns are
//! its own and are not shared.
//!
//! # A transaction across leaders
//!
//! A record's state is the version a reader at the snapshot sees, which is not
//! always its newest: a version of a transaction the snapshot does not show
//! yet is passed over for the one under it, and an intent is not a value. Those
//! travel after the records, with what the reader decided from, in [`tail`]
//! (ADR-0112 D9a).

mod tail;

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use tessari_encoding::{
    LogId, LogRecord, Mutation, RecordKey, RecordValue, StampedValue, StoreKey, StoreValue,
    TopicHeadKey,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence, ShardId, TableId};

use crate::catalog::ShardMap;
use crate::catalog::system;
use crate::error::Result;
use crate::store::Store;
use crate::transaction::Transaction;

/// How many stored keys one page of the record walk reads.
const PAGE: usize = 1_000;

/// A topic's last-given position, carried beside the records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopicHead {
    /// The namespace the topic belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The topic.
    pub table: TableId,
    /// The last position the topic has given.
    pub last: u64,
}

/// A read of a store's state at one version, handed out a chunk at a time.
pub struct StateReader<'a> {
    store: &'a Store,
    /// The part of the store read: every record a collect at this reach would
    /// carry, by the same rule (`catalog::carried_to`), and nothing else.
    within: Reach,
    view: Transaction<'a>,
    version: Sequence,
    positions: Vec<(LogId, Sequence)>,
    /// The key the walk continues after; `None` before the first page.
    after: Option<Key>,
    /// The record whose version at or below the snapshot has been decided.
    decided: Option<Vec<u8>>,
    finished: bool,
    maps: BTreeMap<TableId, Option<Arc<ShardMap>>>,
    /// A record read past the end of the chunk it could not join.
    held: Option<Mutation>,
    /// What follows the records of transactions across leaders.
    tail: tail::Tail,
}

impl<'a> StateReader<'a> {
    /// Begin reading `store`'s state as a peer subscribed at `within` would be
    /// given it; [`Reach::Store`] is all of it. See [`Store::read_state`].
    pub(crate) fn open(store: &'a Store, within: Reach) -> Result<Self> {
        loop {
            let before = store.committed_version()?;
            let mut positions = Vec::new();
            for log in store.logs()? {
                // The logs a collect at this reach may read: inside it, or
                // above it carrying the definitions it needs.
                if within.contains(log.home) || log.home.contains(within) {
                    positions.push((log, store.committed_tail(log)?));
                }
            }
            let view = store.begin_at(before)?;
            if store.committed_version()? == before {
                return Ok(Self {
                    store,
                    within,
                    view,
                    version: before,
                    positions,
                    after: None,
                    decided: None,
                    finished: false,
                    maps: BTreeMap::new(),
                    held: None,
                    tail: tail::Tail::default(),
                });
            }
        }
    }
}

impl StateReader<'_> {
    /// The store version this state is read at.
    #[must_use]
    pub const fn version(&self) -> Sequence {
        self.version
    }

    /// Where each log stood at that version.
    #[must_use]
    pub fn positions(&self) -> &[(LogId, Sequence)] {
        &self.positions
    }

    /// Every topic's last-given position.
    ///
    /// # Errors
    ///
    /// Returns an error when the index keyspace cannot be read.
    pub fn topic_heads(&self) -> Result<Vec<TopicHead>> {
        let request = ScanRequest {
            keyspace: TopicHeadKey::keyspace(),
            range: KeyRange::prefix(&[TopicHeadKey::KIND.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut heads = Vec::new();
        for (key, value) in self.store.backend().scan(&request)? {
            let key = TopicHeadKey::decode(key.as_slice())?;
            if !self
                .within
                .contains(Reach::Database(key.namespace, key.database))
            {
                continue;
            }
            heads.push(TopicHead {
                namespace: key.namespace,
                database: key.database,
                table: key.table,
                last: Sequence::decode(value.as_slice())?.get(),
            });
        }
        Ok(heads)
    }

    /// The next records of the state, at most `limit` of them, in key order.
    ///
    /// A record is its newest version at or below the version read; one whose
    /// newest such version is a tombstone is absent and is not read. `None` once
    /// every record has been read.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or a stored value decoded.
    pub fn next_chunk(&mut self, limit: usize) -> Result<Option<LogRecord>> {
        let mut mutations: Vec<Mutation> = self.held.take().into_iter().collect();
        while mutations.len() < limit && !self.finished && self.held.is_none() {
            let lower = self.after.clone().map_or(Bound::Unbounded, Bound::Excluded);
            let request = ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::from_bounds(lower, Bound::Unbounded),
                direction: ScanDirection::Forward,
                limit: Some(PAGE),
            };
            let page = self.store.backend().sweep(&request)?;
            // The last page, but only once it has been walked to its end: a
            // chunk that stops part-way resumes after the key it stopped at.
            let last = page.len() < PAGE;
            for (key, value) in page {
                self.after = Some(key.clone());
                let stored = RecordKey::decode(key.as_slice())?;
                let prefix = RecordKey::versions_prefix(
                    stored.namespace,
                    stored.database,
                    stored.table,
                    &stored.id,
                );
                // Versions sort newest-first, so the first one at or below the
                // snapshot decides the record and the rest are older history.
                if self.decided.as_deref() == Some(prefix.as_slice())
                    || stored.version > self.version
                {
                    continue;
                }
                if derived(&stored) {
                    self.decided = Some(prefix);
                    continue;
                }
                let value = StampedValue::decode(value.as_slice())?;
                // A version of a transaction this snapshot does not show is
                // passed over for the one under it, as a reader passes over
                // it; an intent the snapshot does show is the record, but
                // restored as the intent it is. Both go after the records.
                let shown = !self.view.passes_over(&value)?;
                let provisional = crate::intents::is_intent(&value);
                if shown {
                    self.decided = Some(prefix);
                }
                if !shown || provisional {
                    let at = stored.version;
                    if let Some(mutation) = self.carried(stored, value)? {
                        self.tail.hold(at, mutation);
                    }
                    continue;
                }
                if matches!(value.value(), RecordValue::Tombstone) {
                    continue;
                }
                let Some(mutation) = self.carried(stored, value)? else {
                    continue;
                };
                // The catalog is restored in chunks of its own, and all of it
                // before any other record. A restore derives each chunk against
                // what is already committed, as an apply does, so an index or an
                // analyzer defined in the same chunk as the records it describes
                // would derive nothing for them — the ordering a log replay gets
                // for free from the order the definitions were committed in.
                let catalog = |held: &Mutation| held.namespace == system::SYSTEM_NAMESPACE;
                if mutations
                    .last()
                    .is_some_and(|last| catalog(last) != catalog(&mutation))
                {
                    self.held = Some(mutation);
                    break;
                }
                mutations.push(mutation);
            }
            if last && self.held.is_none() {
                self.finished = true;
            }
        }
        if mutations.is_empty() {
            return self.tail.next(self.store, self.version, self.within);
        }
        Ok(Some(LogRecord::new(mutations)))
    }

    /// `stored` as a mutation of the state, or `None` when it falls outside
    /// the part of the store read.
    fn carried(&mut self, stored: RecordKey, value: StampedValue) -> Result<Option<Mutation>> {
        let shard = self.shard_of(&stored)?;
        let mutation = Mutation {
            namespace: stored.namespace,
            database: stored.database,
            table: stored.table,
            id: stored.id,
            shard,
            value,
        };
        if self.within != Reach::Store
            && !crate::catalog::carried_to(&mutation)?.reaches(self.within)
        {
            return Ok(None);
        }
        Ok(Some(mutation))
    }

    /// The shard of a split table a record falls in, as the catalog at the
    /// version read says.
    fn shard_of(&mut self, stored: &RecordKey) -> Result<Option<ShardId>> {
        if stored.namespace == system::SYSTEM_NAMESPACE {
            return Ok(None);
        }
        let map = match self.maps.get(&stored.table) {
            Some(known) => known.clone(),
            None => {
                let found = crate::catalog::Catalog::new(&mut self.view)
                    .table(stored.table)?
                    .and_then(|definition| definition.shards)
                    .map(Arc::new);
                self.maps.insert(stored.table, found.clone());
                found
            }
        };
        Ok(map.map(|map| map.shard_of(&stored.id)))
    }
}

/// Whether a record is written by a `maintain` function rather than by a
/// statement, and so comes back when the records around it are restored.
pub(crate) fn derived(stored: &RecordKey) -> bool {
    stored.namespace == system::SYSTEM_NAMESPACE
        && stored.database == system::SYSTEM_DATABASE
        && stored.table == system::RECORD_COUNTS
}
