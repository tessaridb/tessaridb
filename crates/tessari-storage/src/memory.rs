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
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, Precondition, Value, WriteBatch, WriteOp};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::catalog::system::{RECORD_COUNTS, SYSTEM_DATABASE, SYSTEM_NAMESPACE};

mod dropped;
mod routed;
mod space;

pub(crate) use space::SpaceMemory;

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

/// One held space: its own keys, and the prefix of its count record's
/// versions, which live in the system counts table and so carry no place of
/// the space's own.
#[derive(Debug)]
pub(crate) struct Space {
    count_versions: Vec<u8>,
    memory: SpaceMemory,
}

impl Space {
    /// The space's keys.
    pub(crate) fn memory(&self) -> &SpaceMemory {
        &self.memory
    }
}

/// The spaces this node holds in memory: a map from each held table to the
/// map of its own keys.
#[derive(Debug, Default)]
pub(crate) struct HeldSpaces {
    /// Whether any space is held, so a store with none pays one load per key.
    any: AtomicBool,
    /// Each held table and its keys.
    tables: DashMap<Place, Arc<Space>>,
    /// Spaces a drop has named, and the version they were first seen gone at
    /// (`dropped.rs`).
    dropping: DashMap<Place, Option<Sequence>>,
    /// Whether `dropping` names any, so a commit with none pays one load.
    any_dropping: AtomicBool,
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
                Arc::new(Space {
                    count_versions: RecordKey::versions_prefix(
                        SYSTEM_NAMESPACE,
                        SYSTEM_DATABASE,
                        RECORD_COUNTS,
                        &RecordId::Int(i64::from(table.get())),
                    ),
                    memory: SpaceMemory::new(),
                })
            });
        // Release: pairs with the Acquire in `routes`, after the insert.
        self.any.store(true, Ordering::Release);
    }

    /// Every held space, as it stands.
    pub(crate) fn all(&self) -> Vec<Arc<Space>> {
        self.tables
            .iter()
            .map(|held| Arc::clone(held.value()))
            .collect()
    }

    /// The held space `key` belongs to, if any.
    fn routes(&self, key: &[u8]) -> Option<Arc<Space>> {
        // Acquire: pairs with the Release in `hold`.
        if !self.any.load(Ordering::Acquire) {
            return None;
        }
        let tag = key.first()?;
        let at = if *tag == KeyKind::Record.tag() || *tag == KeyKind::ModifiedOrder.tag() {
            PLACE_AT
        } else if *tag == KeyKind::ExpiryIndex.tag() {
            EXPIRY_PLACE_AT
        } else {
            return None;
        };
        let place = place_in(key, at)?;
        if *tag == KeyKind::Record.tag() && place == counts_place() {
            return self
                .tables
                .iter()
                .find(|held| key.starts_with(held.value().count_versions.as_slice()))
                .map(|held| Arc::clone(held.value()));
        }
        self.tables.get(&place).map(|held| Arc::clone(held.value()))
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
    /// The version key, when the newest commit wrote only held spaces.
    version: MemoryBackend,
    held: Arc<HeldSpaces>,
}

impl Routed {
    pub(crate) fn new(disk: Arc<dyn KvBackend>, held: Arc<HeldSpaces>) -> Self {
        Self {
            disk,
            version: MemoryBackend::new(),
            held,
        }
    }

    /// Where `key` lives: its held space, or the store's backend.
    fn side(&self, key: &[u8]) -> Side<'_> {
        match self.held.routes(key) {
            Some(space) => Side::Held(space),
            None => Side::Disk(&*self.disk),
        }
    }

    /// Where `range` lives: the side of its start, so a range across tables —
    /// which starts with no table — reads the store's backend alone.
    fn side_of(&self, range: &KeyRange) -> Side<'_> {
        match range.start() {
            std::ops::Bound::Included(start) | std::ops::Bound::Excluded(start) => {
                self.side(start.as_slice())
            }
            std::ops::Bound::Unbounded => Side::Disk(&*self.disk),
        }
    }

    /// The version key, as the larger of the two sides.
    async fn version(&self) -> tessari_kv::Result<Option<Value>> {
        let key = VersionPositionKey.encode();
        let keyspace = VersionPositionKey::keyspace();
        let disk = self.disk.get(keyspace, &key).await?;
        let memory = self.version.get(keyspace, &key).await?;
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
            let mut held: Vec<(Arc<Space>, WriteBatch)> = Vec::new();
            let mut version = None;
            for op in batch.ops() {
                if *op.key() == version_key {
                    version = Some(op.clone());
                    if let WriteOp::Put { value, .. } = op {
                        current = Some(value.clone());
                    }
                } else if let Some(space) = self.held.routes(op.key().as_slice()) {
                    match held.iter_mut().find(|(seen, _)| Arc::ptr_eq(seen, &space)) {
                        Some((_, batch)) => *batch = with(std::mem::take(batch), op.clone()),
                        None => held.push((space, with(WriteBatch::new(), op.clone()))),
                    }
                } else {
                    disk = with(disk, op.clone());
                }
            }
            // The version goes where the rest of the commit goes: to disk with
            // anything durable, to memory with a commit that wrote only held
            // records.
            let mut memory_version = WriteBatch::new();
            if let Some(version) = version {
                if disk.ops().is_empty() {
                    memory_version = with(memory_version, version);
                } else {
                    disk = with(disk, version);
                }
            }
            parts.push(Parts {
                disk,
                held,
                version: memory_version,
            });
        }
        Ok((parts, None))
    }

    /// [`Self::split`] for one batch: its halves, or its version conflict.
    async fn one(&self, batch: WriteBatch) -> tessari_kv::Result<Option<Parts>> {
        let (mut parts, refused) = self.split(vec![batch]).await?;
        match refused {
            Some(refused) => Err(refused),
            None => Ok(parts.pop()),
        }
    }

    /// A batch's memory half: each held space's part, then the version.
    async fn land_held(&self, parts: Parts) -> tessari_kv::Result<()> {
        for (space, batch) in parts.held {
            space.memory.apply(batch).await?;
        }
        if !parts.version.is_empty() {
            self.version.apply(parts.version).await?;
        }
        Ok(())
    }
}

/// One side of the router, held for the length of one call.
enum Side<'a> {
    Disk(&'a dyn KvBackend),
    Held(Arc<Space>),
}

impl Side<'_> {
    fn backend(&self) -> &dyn KvBackend {
        match self {
            Self::Disk(disk) => *disk,
            Self::Held(space) => &space.memory,
        }
    }
}

/// One batch's parts: the store's backend's, each held space's, and the
/// version key when the commit wrote only held spaces.
struct Parts {
    disk: WriteBatch,
    held: Vec<(Arc<Space>, WriteBatch)>,
    version: WriteBatch,
}

impl Parts {
    fn holds_memory(&self) -> bool {
        !self.held.is_empty() || !self.version.is_empty()
    }
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

#[cfg(test)]
mod tests;
