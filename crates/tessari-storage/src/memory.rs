//! Spaces held in node memory only (ADR-0130, G079 W5b).
//!
//! A space declared `IN MEMORY` is a space in every respect a reader or a writer
//! can see — the same transactions, versions, expiry, limit and walks — except
//! where its bytes live. They live here, in a [`MemoryBackend`] beside the
//! store's own backend, which this type routes between.
//!
//! # Which keys are routed
//!
//! A key of a held space's record (`0x01`), expiry entry (`0x1a`, its table after
//! the instant), modified-order entry (`0x1b`) and its per-table count — a
//! record of the system counts table whose id is the space's table — goes to
//! memory; every other key to the store's backend. A scan goes to the side of
//! its START, which for a range inside one table or one record names that table;
//! a range across tables starts with no table and reads the store's backend
//! alone, which is exactly what keeps a held space out of a snapshot, a script
//! backup and a follower's re-seed copy.
//!
//! # One version counter
//!
//! A commit writing only held records lands at the next store version with no
//! log entry, so the version key it writes lands here too. This type answers the
//! version key as the larger of its two sides and checks a batch's version
//! precondition itself, against that answer: one process owns a store and the
//! write gate orders its writers, so the check the backend would have made
//! atomically is made under the same turn. A group lands its disk part first —
//! one sync — and its memory part after, so a failed sync leaves memory as it
//! was. After a restart memory is empty and the backend's key is the last
//! durable version, which is what every later commit counts on from.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;
use tessari_encoding::{KeyKind, RecordKey, StoreKey, StoreValue, VersionPositionKey};
use tessari_kv::{
    GroupFuture, Key, KeyRange, Keyspace, KvBackend, KvFuture, MemoryBackend, Precondition,
    ScanRequest, Value, WriteBatch, WriteOp,
};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::catalog::system::{RECORD_COUNTS, SYSTEM_DATABASE, SYSTEM_NAMESPACE};

/// Where a record or modified-order key names its namespace, database and
/// table: after the tag.
const PLACE_AT: usize = 1;
/// Where an expiry entry names them: after the tag and the instant.
const EXPIRY_PLACE_AT: usize = 9;
/// The length of a namespace, database and table identifier together. A table
/// identifier is unique only inside its database — a user table and a catalog
/// table may share one — so a table is named by all three.
const PLACE_LEN: usize = 12;

/// A table by its namespace, database and table identifiers, as a key spells
/// them.
type Place = [u8; PLACE_LEN];

/// The spaces this node holds in memory, by table.
#[derive(Debug, Default)]
pub(crate) struct HeldSpaces {
    /// Whether any space is held, so a store with none pays one load per key.
    any: AtomicBool,
    /// Each held table and the prefix of its count record's versions.
    tables: DashMap<Place, Vec<u8>>,
}

impl HeldSpaces {
    /// Hold `table` in memory from now on.
    ///
    /// Called by the commit that first writes it, under the write gate, before
    /// its batch is built — so no key of the table is ever routed to disk.
    pub(crate) fn hold(&self, namespace: NamespaceId, database: DatabaseId, table: TableId) {
        self.tables
            .entry(place_of(namespace, database, table))
            .or_insert_with(|| {
                RecordKey::versions_prefix(
                    SYSTEM_NAMESPACE,
                    SYSTEM_DATABASE,
                    RECORD_COUNTS,
                    &RecordId::Int(i64::from(table.get())),
                )
            });
        // Release: pairs with the Acquire in `routes`, after the insert.
        self.any.store(true, Ordering::Release);
    }

    /// Whether `key` belongs to a held space.
    fn routes(&self, key: &[u8]) -> bool {
        // Acquire: pairs with the Release in `hold`.
        if !self.any.load(Ordering::Acquire) {
            return false;
        }
        let Some(tag) = key.first() else {
            return false;
        };
        let at = if *tag == KeyKind::Record.tag() || *tag == KeyKind::ModifiedOrder.tag() {
            PLACE_AT
        } else if *tag == KeyKind::ExpiryIndex.tag() {
            EXPIRY_PLACE_AT
        } else {
            return false;
        };
        let Some(place) = place_in(key, at) else {
            return false;
        };
        if *tag == KeyKind::Record.tag() && place == counts_place() {
            return self
                .tables
                .iter()
                .any(|held| key.starts_with(held.value().as_slice()));
        }
        self.tables.contains_key(&place)
    }
}

/// The place at `at` in `key`, when the key is long enough to hold one.
fn place_in(key: &[u8], at: usize) -> Option<Place> {
    let end = at.checked_add(PLACE_LEN)?;
    key.get(at..end)?.try_into().ok()
}

/// Where the system counts table is, as a key spells it.
fn counts_place() -> Place {
    place_of(SYSTEM_NAMESPACE, SYSTEM_DATABASE, RECORD_COUNTS)
}

/// A table's place as a key spells it: three big-endian identifiers.
fn place_of(namespace: NamespaceId, database: DatabaseId, table: TableId) -> Place {
    let mut place = [0_u8; PLACE_LEN];
    place[..4].copy_from_slice(&namespace.get().to_be_bytes());
    place[4..8].copy_from_slice(&database.get().to_be_bytes());
    place[8..].copy_from_slice(&table.get().to_be_bytes());
    place
}

/// The store's backend and node memory, each key on one of them.
#[derive(Debug)]
pub(crate) struct Routed {
    disk: Arc<dyn KvBackend>,
    memory: MemoryBackend,
    held: Arc<HeldSpaces>,
}

impl Routed {
    pub(crate) fn new(disk: Arc<dyn KvBackend>, held: Arc<HeldSpaces>) -> Self {
        Self {
            disk,
            memory: MemoryBackend::new(),
            held,
        }
    }

    /// Node memory alone, for the one reader that walks every table's entries
    /// and must find the held ones too: the expiry removal pass.
    pub(crate) fn memory(&self) -> &MemoryBackend {
        &self.memory
    }

    fn side(&self, key: &[u8]) -> &dyn KvBackend {
        if self.held.routes(key) {
            &self.memory
        } else {
            &*self.disk
        }
    }

    fn side_of(&self, range: &KeyRange) -> &dyn KvBackend {
        if self.in_memory(range) {
            &self.memory
        } else {
            &*self.disk
        }
    }

    /// Whether `range` reads a held space: its start names one.
    fn in_memory(&self, range: &KeyRange) -> bool {
        match range.start() {
            std::ops::Bound::Included(start) | std::ops::Bound::Excluded(start) => {
                self.held.routes(start.as_slice())
            }
            std::ops::Bound::Unbounded => false,
        }
    }

    /// The version key, as the larger of the two sides.
    async fn version(&self) -> tessari_kv::Result<Option<Value>> {
        let key = VersionPositionKey.encode();
        let keyspace = VersionPositionKey::keyspace();
        let disk = self.disk.get(keyspace, &key).await?;
        let memory = self.memory.get(keyspace, &key).await?;
        Ok(match (disk, memory) {
            (Some(disk), Some(memory)) => {
                if decoded(&memory) > decoded(&disk) {
                    Some(memory)
                } else {
                    Some(disk)
                }
            }
            (disk, None) => disk,
            (None, memory) => memory,
        })
    }

    /// Split `batches` into what each side lands, checking every version
    /// precondition against the version as it stands after the batches before.
    ///
    /// Answers the parts of the batches that hold, and the conflict of the
    /// first one that does not.
    async fn split(
        &self,
        batches: Vec<WriteBatch>,
    ) -> tessari_kv::Result<(Vec<Parts>, Option<tessari_kv::Error>)> {
        let version_key = VersionPositionKey.encode();
        let mut current = self.version().await?;
        let mut parts = Vec::with_capacity(batches.len());
        for batch in batches {
            let mut disk = WriteBatch::new();
            for precondition in batch.preconditions() {
                if *precondition.key() == version_key {
                    if !precondition.is_satisfied_by(current.as_ref()) {
                        return Ok((
                            parts,
                            Some(tessari_kv::Error::Conflict {
                                keyspace: precondition.keyspace().to_string(),
                                key: version_key,
                            }),
                        ));
                    }
                } else {
                    disk = expect(disk, precondition);
                }
            }
            let mut memory = WriteBatch::new();
            let mut version = None;
            for op in batch.ops() {
                if *op.key() == version_key {
                    version = Some(op.clone());
                    if let WriteOp::Put { value, .. } = op {
                        current = Some(value.clone());
                    }
                } else if self.held.routes(op.key().as_slice()) {
                    memory = with(memory, op.clone());
                } else {
                    disk = with(disk, op.clone());
                }
            }
            // The version goes where the rest of the commit goes: to disk with
            // anything durable, to memory with a commit that wrote only held
            // records.
            if let Some(version) = version {
                if disk.ops().is_empty() {
                    memory = with(memory, version);
                } else {
                    disk = with(disk, version);
                }
            }
            parts.push(Parts { disk, memory });
        }
        Ok((parts, None))
    }
}

impl Routed {
    /// [`Self::split`] for one batch: its halves, or its version conflict.
    async fn one(&self, batch: WriteBatch) -> tessari_kv::Result<Option<Parts>> {
        let (mut parts, refused) = self.split(vec![batch]).await?;
        match refused {
            Some(refused) => Err(refused),
            None => Ok(parts.pop()),
        }
    }
}

/// One batch's two halves.
struct Parts {
    disk: WriteBatch,
    memory: WriteBatch,
}

fn decoded(value: &Value) -> Sequence {
    Sequence::decode(value.as_slice()).unwrap_or(Sequence::ZERO)
}

fn with(batch: WriteBatch, op: WriteOp) -> WriteBatch {
    match op {
        WriteOp::Put {
            keyspace,
            key,
            value,
        } => batch.put(keyspace, key, value),
        WriteOp::Delete { keyspace, key } => batch.delete(keyspace, key),
    }
}

fn expect(batch: WriteBatch, precondition: &Precondition) -> WriteBatch {
    match precondition.clone() {
        Precondition::ValueIs {
            keyspace,
            key,
            expected,
        } => batch.expect_value(keyspace, key, expected),
        Precondition::Absent { keyspace, key } => batch.expect_absent(keyspace, key),
        Precondition::Exists { keyspace, key } => batch.expect_exists(keyspace, key),
    }
}

impl KvBackend for Routed {
    fn name(&self) -> &'static str {
        self.disk.name()
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        if *key == VersionPositionKey.encode() {
            return Box::pin(self.version());
        }
        self.side(key.as_slice()).get(keyspace, key)
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        self.side_of(&request.range).scan(request)
    }

    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        self.side_of(&request.range).sweep(request)
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        // Whole to one side when every range is on it, so a backend that answers
        // many ranges in one ask still does.
        let held: Vec<bool> = ranges.iter().map(|range| self.in_memory(range)).collect();
        if held.iter().all(|held| !held) {
            return self.disk.first_of_each(keyspace, ranges);
        }
        if held.iter().all(|held| *held) {
            return self.memory.first_of_each(keyspace, ranges);
        }
        Box::pin(async move {
            let mut found = Vec::with_capacity(ranges.len());
            for range in ranges {
                let side = self.side_of(range);
                found.push(
                    side.first_of_each(keyspace, std::slice::from_ref(range))
                        .await?
                        .pop()
                        .flatten(),
                );
            }
            Ok(found)
        })
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        self.side_of(range).count(keyspace, range)
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        if *key == VersionPositionKey.encode() {
            return Box::pin(async move { Ok(self.version().await?.is_some()) });
        }
        self.side(key.as_slice()).contains(keyspace, key)
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        self.side_of(range).delete_range(keyspace, range)
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move {
            let Some(parts) = self.one(batch).await? else {
                return Ok(());
            };
            if !parts.disk.is_empty() {
                self.disk.apply(parts.disk).await?;
            }
            if !parts.memory.is_empty() {
                self.memory.apply(parts.memory).await?;
            }
            Ok(())
        })
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move {
            let Some(parts) = self.one(batch).await? else {
                return Ok(());
            };
            if !parts.disk.is_empty() {
                self.disk.apply_unsynced(parts.disk).await?;
            }
            if !parts.memory.is_empty() {
                self.memory.apply(parts.memory).await?;
            }
            Ok(())
        })
    }

    fn sync_applied(&self) -> KvFuture<'_, ()> {
        self.disk.sync_applied()
    }

    fn apply_group(&self, batches: Vec<WriteBatch>) -> GroupFuture<'_> {
        Box::pin(async move {
            let (parts, refused) = match self.split(batches).await {
                Ok(split) => split,
                Err(failure) => return (0, Err(failure)),
            };
            // The disk halves land first, as one group, and only the batches
            // whose disk half landed have their memory half applied.
            let mut durable = Vec::new();
            let mut owners = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                if !part.disk.is_empty() {
                    durable.push(part.disk.clone());
                    owners.push(index);
                }
            }
            let (on_disk, failed) = if durable.is_empty() {
                (0, Ok(()))
            } else {
                self.disk.apply_group(durable).await
            };
            let landed = match &failed {
                Ok(()) => parts.len(),
                Err(_) => owners.get(on_disk).copied().unwrap_or(parts.len()),
            };
            for part in parts.into_iter().take(landed) {
                if !part.memory.is_empty()
                    && let Err(failure) = self.memory.apply(part.memory).await
                {
                    return (0, Err(failure));
                }
            }
            match (failed, refused) {
                (Err(failure), _) => (landed, Err(failure)),
                (Ok(()), Some(refused)) => (landed, Err(refused)),
                (Ok(()), None) => (landed, Ok(())),
            }
        })
    }

    fn groups_writes(&self) -> bool {
        self.disk.groups_writes()
    }

    fn background_errors(&self) -> KvFuture<'_, u64> {
        self.disk.background_errors()
    }

    fn close(self: Arc<Self>) -> KvFuture<'static, ()> {
        Arc::clone(&self.disk).close()
    }
}

#[cfg(test)]
mod tests;
