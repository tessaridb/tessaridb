//! The columnar projection's commit-batch derivation (the design record).
//!
//! One link in `Settle::derived`'s chain, beside `crate::index`: the log
//! record a commit carried is replayed against the committed state, and the
//! chunk, map and pointer writes it implies land in the same batch — the
//! index pattern, elevated from per-key entries to per-column blocks
//! (ADR-0137). One writer (the commit apply), atomicity and crash recovery
//! inherited, no second projection job, ever (§5.3).
//!
//! # What one mutation projects
//!
//! - **Create / insert** — the record's value per family appends to the
//!   family's open chunk; the `'M'` map entry records which chunk holds the
//!   row. A schemafull record's absent fields land as null slots; a
//!   schemaless record projects the keys it carries (the design record, refined by
//!   the design record: a record covers the families it has values for).
//! - **Update** — a field whose value changed is a row replaced in the open
//!   chunk, or, when the row sits in a sealed chunk, a copy-on-write rewrite
//!   of that chunk (the value removed) and an append to the open one (§5.2).
//!   A field whose value is unchanged writes nothing.
//! - **Delete** — nothing. The row tombstone is the authority for visibility
//!   and a scan re-evaluates it (§5.2, §5.4); rewriting the chunk would be
//!   the vacuum §5.4 defers . The `'M'` entry stays: it points at
//!   rows a scan will filter, and costs nothing a vacuum would not already
//!   reclaim.
//!
//! # Exactness
//!
//! Every write here is derived from the same record bytes the row path
//! applies, in the same batch — a projection bug can cost a wrong chunk,
//! never a wrong answer, because the row stays the authority and the scan
//! re-evaluates candidates (§6.1).

use std::collections::BTreeMap;

use crate::RecordAddress;
use tessari_encoding::{
    ChunkKey, ColumnFamily, MapKey, PointerKey, RecordValue, StoreKey, decode_payload,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, Value, WriteBatch};
use tessari_types::{
    DatabaseId, NamespaceId, RecordId, Sequence, TableId, Value as RecordValueType,
};

use crate::catalog::{Catalog, TableDefinition};
use crate::error::Result;
use crate::store::Store;

use super::chunk::{local_of_seq, split_seq};
use super::writer::{ChunkWriter, remove_row};

/// One mutation with its before/after decoded — what the projection loop
/// consumes, and what the `SORT BY` pass orders.
type Prepared<'a> = (
    &'a tessari_encoding::Mutation,
    Option<BTreeMap<String, RecordValueType>>,
    Option<BTreeMap<String, RecordValueType>>,
);

/// The partition a record's values route to, `0` when the table names no
/// partitioning. The hash is the murmur2 both consumers share, over the
/// payload encoding of the partition field's value — the normal form
/// comparison reduces to, so equal values route equal. A null or absent
/// value routes to partition 0 (the design record: analytics narrows, it does not
/// fail); a field the table does not declare is a defective definition and
/// refuses the write.
fn partition_of(
    definition: &TableDefinition,
    object: Option<&BTreeMap<String, RecordValueType>>,
) -> Result<u64> {
    let Some(partitioning) = &definition.partitions else {
        return Ok(0);
    };
    let value = object.and_then(|fields| fields.get(&partitioning.by));
    let Some(value) =
        value.filter(|value| !matches!(value, RecordValueType::Null | RecordValueType::None))
    else {
        return Ok(0);
    };
    let payload = tessari_encoding::encode_payload(value).into_bytes();
    let hashed = u64::from(tessari_encoding::murmur2(&payload) & 0x7fff_ffff);
    let count = u64::from(partitioning.count);
    // The count is a definition-time constant of at least one (the define
    // guard refuses zero), so the remainder cannot fail.
    Ok(hashed.checked_rem(count).unwrap_or(0))
}

/// A lane's sealed-range statistics: the [min, max] over every SEALED chunk
/// of the lane (the design record, OLAP ref 02 M2 — write-path invariant,
/// computed by `project_table`'s flush in the same batch as the chunks).
///
/// Conservatism rules, all in the direction of reading more, never less:
/// the merge only ever widens; a sealed chunk without range statistics (a
/// NaN disabled it, or the lane's history predates this wave) leaves the
/// lane without statistics, and absence is sticky — a lane that once could
/// hold anything keeps that doubt for good. The open chunk is never
/// covered: the scan consults it every time.
#[derive(Clone, Debug)]
pub(crate) struct LaneRange {
    /// The least sealed value under the store's one declared order.
    pub(crate) min: tessari_types::Value,
    /// The greatest sealed value under the same order.
    pub(crate) max: tessari_types::Value,
}

/// One lane of a family's pointer: where the open chunk is, and what the
/// sealed chunks statistically span.
#[derive(Clone, Debug)]
pub(crate) struct LanePointer {
    /// The seq the open chunk has advanced to; chunks with a smaller local
    /// seq are sealed.
    pub(crate) open: u64,
    /// The sealed-range statistics, or `None` when absent.
    pub(crate) range: Option<LaneRange>,
    /// Whether the lane's statistics are disabled for good: a sealed chunk
    /// a NaN disabled, or a lane whose history predates this wave and whose
    /// sealed chunks are unknown. Sticky — the flush never upgrades a
    /// disabled lane, because a range over unknown history would understate
    /// what the lane can hold. A lane born under this wave is not disabled
    /// until a NaN says so, so its young-table `None` upgrades freely.
    pub(crate) disabled: bool,
}

/// One sealed chunk's contribution to its lane's range: what it spans, that
/// it holds nothing to span, or that it disables the lane's range.
#[derive(Clone, Debug)]
pub(crate) enum SealedStat {
    /// The chunk's [min, max].
    Ranged(tessari_types::Value, tessari_types::Value),
    /// The chunk holds no non-null values: no contribution, no disable.
    Empty,
    /// Range statistics were disabled for the chunk (a NaN sat the column):
    /// the chunk can hold anything, so the lane's range is disabled.
    Disabled,
}

/// The pointer value's v2 marker. A v1 value begins with its lane count;
/// four billion lanes is not a value any store can hold, so a v1 parser
/// meets this only by refusing on truncation — old builds refuse new
/// pointers, the documented upgrade direction.
const POINTER_MAGIC: u32 = u32::MAX;

/// `flags` bits in the v2 form: range statistics present, statistics
/// disabled for good.
const FLAG_RANGE: u8 = 0x01;
const FLAG_DISABLED: u8 = 0x02;

/// A family's pointer value: the open chunk seq of every partition lane it
/// holds, plus each lane's sealed-range statistics. Two legacy forms
/// survive: the bare eight-byte form (one lane at partition 0, no
/// statistics) and the v1 lane-count form without statistics — both parse
/// with `range: None`, because absence disables pruning, it never guesses.
///
/// v2 layout: `[magic u32][count u32]` then per lane
/// `[partition u32][open u64][flags u8]` and, when [`FLAG_RANGE`], the
/// `[min][max]` payloads (each a u32 length + the store's own encoding).
pub(crate) fn lanes_to_bytes(lanes: &BTreeMap<u64, LanePointer>) -> Vec<u8> {
    let mut out = POINTER_MAGIC.to_be_bytes().to_vec();
    out.extend_from_slice(&u32::try_from(lanes.len()).unwrap_or(u32::MAX).to_be_bytes());
    for (partition, lane) in lanes {
        out.extend_from_slice(&u32::try_from(*partition).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&lane.open.to_be_bytes());
        let mut flags = 0_u8;
        if lane.range.is_some() {
            flags |= FLAG_RANGE;
        }
        if lane.disabled {
            flags |= FLAG_DISABLED;
        }
        out.push(flags);
        if let Some(range) = &lane.range {
            put_stat(&mut out, &range.min);
            put_stat(&mut out, &range.max);
        }
    }
    out
}

/// One statistic payload: a u32 length + the store's own encoding, the same
/// shape the chunk head uses.
fn put_stat(out: &mut Vec<u8>, value: &tessari_types::Value) {
    let encoded = tessari_encoding::encode_payload(value).into_bytes();
    out.extend_from_slice(
        &u32::try_from(encoded.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(&encoded);
}

/// The lanes a pointer value names — [`lanes_to_bytes`] read back. Legacy
/// forms (the bare eight-byte open seq, the v1 lane-count form) parse with
/// no range statistics; anything cut short refuses rather than guesses.
pub(crate) fn lanes_from_bytes(bytes: &[u8]) -> Result<BTreeMap<u64, LanePointer>> {
    let mut lanes = BTreeMap::new();
    if bytes.len() == 8 {
        lanes.insert(
            0,
            LanePointer {
                open: u64::from_be_bytes(bytes.try_into().map_err(|_| {
                    crate::error::Error::ColumnarTruncated {
                        needed: 8,
                        len: bytes.len(),
                    }
                })?),
                range: None,
                // The bare form predates lane statistics: the lane's sealed
                // history is unknown, and unknown is disabled.
                disabled: true,
            },
        );
        return Ok(lanes);
    }
    if bytes.len() < 4 {
        return Err(crate::error::Error::ColumnarTruncated {
            needed: 4,
            len: bytes.len(),
        });
    }
    let first = u32::from_be_bytes(bytes[..4].try_into().map_err(|_| {
        crate::error::Error::ColumnarTruncated {
            needed: 4,
            len: bytes.len(),
        }
    })?);
    if first == POINTER_MAGIC {
        return lanes_from_v2(bytes);
    }
    lanes_from_v1(bytes)
}

/// The v2 form past the magic.
fn lanes_from_v2(bytes: &[u8]) -> Result<BTreeMap<u64, LanePointer>> {
    let mut lanes = BTreeMap::new();
    let mut cursor = super::codec::Cursor::new(bytes);
    let magic = cursor.u32()?;
    debug_assert_eq!(magic, POINTER_MAGIC);
    let count = cursor.u32()?;
    for _ in 0..count {
        let partition = u64::from(cursor.u32()?);
        let open_bytes = cursor.take(8)?;
        let open = u64::from_be_bytes(open_bytes.try_into().map_err(|_| {
            crate::error::Error::ColumnarTruncated {
                needed: 8,
                len: open_bytes.len(),
            }
        })?);
        let flags = cursor.u8()?;
        let range = if flags & FLAG_RANGE != 0 {
            let min = read_stat(&mut cursor)?;
            let max = read_stat(&mut cursor)?;
            Some(LaneRange { min, max })
        } else {
            None
        };
        lanes.insert(
            partition,
            LanePointer {
                open,
                range,
                disabled: flags & FLAG_DISABLED != 0,
            },
        );
    }
    cursor.finish()?;
    Ok(lanes)
}

/// The v1 lane-count form, without statistics.
fn lanes_from_v1(bytes: &[u8]) -> Result<BTreeMap<u64, LanePointer>> {
    let mut lanes = BTreeMap::new();
    let mut cursor = super::codec::Cursor::new(bytes);
    let count = cursor.u32()?;
    for _ in 0..count {
        let partition = u64::from(cursor.u32()?);
        let open_bytes = cursor.take(8)?;
        let open = u64::from_be_bytes(open_bytes.try_into().map_err(|_| {
            crate::error::Error::ColumnarTruncated {
                needed: 8,
                len: open_bytes.len(),
            }
        })?);
        lanes.insert(
            partition,
            LanePointer {
                open,
                range: None,
                // The v1 form predates lane statistics: unknown history is
                // disabled history.
                disabled: true,
            },
        );
    }
    cursor.finish()?;
    Ok(lanes)
}

/// One statistic payload read back.
fn read_stat(cursor: &mut super::codec::Cursor<'_>) -> Result<tessari_types::Value> {
    let len = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    let raw = cursor.take(len)?;
    tessari_encoding::decode_payload(raw).map_err(crate::Error::from)
}

/// Merge the lane's previous range with the contributions of the chunks one
/// commit sealed: the widened [min, max]. [`SealedStat::Empty`] contributes
/// nothing; [`SealedStat::Disabled`] disables the lane's range — one chunk
/// that can hold anything makes the whole lane's range a guess. The sticky
/// half of the rule (never upgrading a lane whose sealed history is
/// unknown) is the caller's, on the strength of the pointer's `disabled`
/// flag.
pub(crate) fn merge_lane_range(
    previous: Option<LaneRange>,
    sealed: &[SealedStat],
) -> Option<LaneRange> {
    let mut merged = previous;
    for stat in sealed {
        match stat {
            SealedStat::Ranged(min, max) => {
                let (min, max) = (min.clone(), max.clone());
                merged = Some(match merged {
                    None => LaneRange { min, max },
                    Some(existing) => LaneRange {
                        min: min.min(existing.min),
                        max: max.max(existing.max),
                    },
                });
            }
            SealedStat::Empty => {}
            SealedStat::Disabled => return None,
        }
    }
    merged
}

/// Derive the columnar projection's writes from one commit's record, into
/// its batch.
pub(crate) async fn maintain(
    store: &Store,
    record: &tessari_encoding::LogRecord,
    batch: WriteBatch,
    version: Sequence,
) -> Result<WriteBatch> {
    let mut by_table: BTreeMap<
        (NamespaceId, DatabaseId, TableId),
        Vec<&tessari_encoding::Mutation>,
    > = BTreeMap::new();
    for mutation in record.mutations() {
        // The catalog's own records are not columnar (their tables are
        // system tables, and projecting them would project the catalog).
        if mutation.namespace == crate::catalog::system::SYSTEM_NAMESPACE {
            continue;
        }
        by_table
            .entry((mutation.namespace, mutation.database, mutation.table))
            .or_default()
            .push(mutation);
    }
    if by_table.is_empty() {
        return Ok(batch);
    }

    let mut view = store.begin_local().await?;
    let mut batch = batch;
    for ((namespace, database, table), mutations) in by_table {
        let Some(definition) = Catalog::new(&mut view)
            .table_shared(table)
            .await?
            .filter(|found| found.namespace == namespace && found.database == database)
        else {
            continue;
        };
        if !matches!(
            definition.storage_kind,
            Some(crate::catalog::StorageKind::Column)
        ) {
            continue;
        }
        batch = project_table(store, &mut view, &definition, &mutations, batch, version).await?
    }
    Ok(batch)
}

/// Project one table's mutations.
async fn project_table(
    store: &Store,
    view: &mut crate::transaction::Transaction<'_>,
    definition: &TableDefinition,
    mutations: &[&tessari_encoding::Mutation],
    batch: WriteBatch,
    version: Sequence,
) -> Result<WriteBatch> {
    let namespace = definition.namespace;
    let database = definition.database;
    let table = definition.id;

    // Declared families are read once per table, the way
    // `index::maintain_from` reads the analyzer once: they are the same
    // value every mutation uses.
    let declared: Vec<(String, ColumnFamily)> = Catalog::new(view)
        .fields_on(namespace, database, table)
        .await?
        .into_iter()
        .map(|field| (field.name.clone(), ColumnFamily::Id(field.id)))
        .collect();

    let mut writers: BTreeMap<(ColumnFamily, u64), ChunkWriter> = BTreeMap::new();
    let mut lanes: BTreeMap<ColumnFamily, BTreeMap<u64, LanePointer>> = BTreeMap::new();
    let mut maps: Vec<(RecordId, Vec<(ColumnFamily, u64)>)> = Vec::new();
    let mut cows: Vec<(u64, ColumnFamily, Vec<u8>)> = Vec::new();

    // Decode each mutation's before/after once, up front: the sort below
    // reads the key from these, and the loop consumes them, so a record's
    // bytes are decoded once however the batch is ordered.
    let mut prepared: Vec<Prepared<'_>> = Vec::with_capacity(mutations.len());
    for mutation in mutations {
        let address = RecordAddress::new(namespace, database, table, mutation.id.clone());
        let old = held_object(view, &address).await?;
        let new = match mutation.value.value() {
            RecordValue::Present(payload) => decode_object(payload)?,
            RecordValue::Tombstone => None,
        };
        prepared.push((mutation, old, new));
    }
    // `SORT BY` (ADR-0135): the batch's rows reach the writers in the sort
    // field's order, so a chunk fills with a tight range of it and the head
    // statistics prune by ranges that mean something. Stable, so equal keys
    // keep the log's own order; skipped — soundly, at the cost of one
    // unsorted batch — when a record repeats in the batch, because a record's
    // own mutations must apply in the order the log holds them and a sort by
    // a value that itself changed between them could reorder exactly those.
    if let Some(field) = &definition.sort {
        let mut seen = std::collections::HashSet::with_capacity(prepared.len());
        let unique = prepared
            .iter()
            .all(|(mutation, ..)| seen.insert(&mutation.id));
        if unique {
            let key_of = |held: &Prepared<'_>| {
                held.2
                    .as_ref()
                    .or(held.1.as_ref())
                    .and_then(|object| object.get(field).cloned())
                    .unwrap_or(RecordValueType::None)
            };
            prepared.sort_by_key(key_of);
        }
    }

    for (mutation, old, new) in prepared {
        // The row's values route to one partition's lane; the values it is
        // losing route from the lane they sit in (the object it held).
        let partition = partition_of(definition, new.as_ref().or(old.as_ref()))?;
        // The families this record touches: every declared family for a
        // schemafull table; every key the old or new object carries for a
        // schemaless one (the design record — a record covers the families it has
        // values for, and a key's disappearance is a null slot in place).
        let touched: Vec<(String, ColumnFamily)> = if !declared.is_empty() {
            declared.clone()
        } else {
            let mut names: BTreeMap<String, ColumnFamily> = BTreeMap::new();
            for object in old.iter().chain(new.iter()) {
                for key in object.keys() {
                    names
                        .entry(key.clone())
                        .or_insert_with(|| ColumnFamily::Name(key.clone()));
                }
            }
            names.into_iter().collect()
        };

        let mut record_map: Vec<(ColumnFamily, u64)> = Vec::new();
        for (name, family) in touched {
            let old_value = old
                .as_ref()
                .and_then(|object| object.get(&name).cloned())
                .unwrap_or(RecordValueType::Null);
            let new_value = new
                .as_ref()
                .and_then(|object| object.get(&name).cloned())
                .unwrap_or(RecordValueType::Null);
            // A new record covers every declared family even when the value
            // it carries is null: a schemafull record's absent fields land
            // as null slots (the module's own contract), because the
            // projection must hold a row for every record of the family —
            // a null slot is how a lane fold or a decode learns the record
            // EXISTS with a null here (the design record: absent and null are two
            // different facts, and conflating them under `old == new` left
            // a written-null record with no key-family row at all — the
            // C6 differential found the vanishing: a `count(*)` grouped by
            // the partition key dropped every null-key row). Only a record
            // that already held this family skips an unchanged value.
            if old.is_some() && old_value == new_value {
                continue;
            }
            let writer = match writers.entry((family.clone(), partition)) {
                std::collections::btree_map::Entry::Occupied(occupied) => occupied.into_mut(),
                std::collections::btree_map::Entry::Vacant(vacant) => {
                    let loaded = load_writer(
                        store,
                        namespace,
                        database,
                        table,
                        &family,
                        partition,
                        lanes.entry(family.clone()).or_default(),
                    )
                    .await?;
                    vacant.insert(loaded)
                }
            };
            if writer.holds(&mutation.id) {
                writer.replace(&mutation.id, new_value.clone());
            } else {
                // The row may sit in a sealed chunk: copy-on-write it away,
                // then append here.
                if let Some((seq, bytes)) =
                    cow_row(store, namespace, database, table, &family, &mutation.id).await?
                {
                    cows.push((seq, family.clone(), bytes));
                }
                writer.append(mutation.id.clone(), new_value.clone());
            }
            record_map.push((family, split_seq(partition, writer.seq())));
        }
        maps.push((mutation.id.clone(), record_map));
    }

    // Flush: copy-on-write rewrites first, then the sealed and open chunks,
    // the pointer moved to each family's open seq with the lane statistics
    // merged forward, and the map entries — all versioned puts in the one
    // batch.
    let mut batch = batch;
    for (seq, family, bytes) in cows {
        // A rewritten sealed chunk keeps its seq and narrows its range:
        // the head index takes the rewritten chunk's own statistics, so the
        // lane stays complete and no read pays a walk for the rewrite.
        if let Ok((_, stats)) = super::chunk::decode_head(&bytes) {
            store.heads().rewritten(
                &super::heads::LaneKey {
                    table,
                    family: family.clone(),
                    partition: seq >> super::chunk::PARTITION_BITS,
                },
                super::chunk::local_of_seq(seq),
                stats,
            );
        }
        batch = batch.put(
            ChunkKey::keyspace(),
            ChunkKey::new(namespace, database, table, family, seq, version).encode(),
            Value::from(bytes),
        );
    }
    for ((family, partition), writer) in writers {
        let open_seq = split_seq(partition, writer.seq());
        // The lane statistics this batch leaves behind. The open chunk is
        // never part of them: `finish` rides it home as the last entry, but
        // appends continue into it in later commits, and a range a later
        // append can exceed would deny rows it has never seen (the design
        // record: the scan consults the open chunk every time).
        let open_local = writer.seq();
        let finished = writer.finish();
        let decoded: Vec<(u64, super::chunk::ChunkStats)> = finished
            .iter()
            .filter(|sealed| sealed.seq != open_local)
            .map(|sealed| Ok((sealed.seq, super::chunk::decode_head(&sealed.bytes)?.1)))
            .collect::<Result<Vec<_>>>()?;
        let contributions: Vec<SealedStat> = decoded
            .iter()
            .map(|(_, stat)| {
                if stat.range_disabled {
                    SealedStat::Disabled
                } else {
                    match stat.min.clone().zip(stat.max.clone()) {
                        Some((min, max)) => SealedStat::Ranged(min, max),
                        None => SealedStat::Empty,
                    }
                }
            })
            .collect();
        let held = lanes.entry(family.clone()).or_default();
        // A lane upgrades from absence only while its sealed history is
        // fully known: it was born under this wave and no NaN has disabled
        // it. Legacy pointers and NaN-disabled lanes read as `disabled` and
        // keep the doubt for good — a range over unknown history would
        // understate what the lane can hold, and an understated range
        // deletes rows.
        let base = match held.get(&partition) {
            Some(lane) if !lane.disabled => lane.range.clone(),
            _ => None,
        };
        let merged = merge_lane_range(base, &contributions);
        let disabled = contributions
            .iter()
            .any(|stat| matches!(stat, SealedStat::Disabled))
            || held.get(&partition).is_some_and(|lane| lane.disabled);
        for sealed in finished {
            batch = batch.put(
                ChunkKey::keyspace(),
                ChunkKey::new(
                    namespace,
                    database,
                    table,
                    family.clone(),
                    split_seq(partition, sealed.seq),
                    version,
                )
                .encode(),
                Value::from(sealed.bytes),
            );
        }
        // The head index extends by exactly the transition that preserves
        // its completeness claim: the chunks this commit sealed, appended to
        // a lane whose base open it still names. Updating here — before the
        // batch lands — is sound in every outcome: a failed commit leaves
        // the pointer behind the bumped open, which reads as stale, and
        // stale is a walk and a reinstall.
        let base_open = held.get(&partition).map_or(0, |lane| lane.open);
        store.heads().sealed(
            &super::heads::LaneKey {
                table,
                family: family.clone(),
                partition,
            },
            base_open,
            open_seq,
            decoded
                .iter()
                .map(|(local, stats)| {
                    (
                        *local,
                        super::heads::HeadEntry {
                            stats: stats.clone(),
                        },
                    )
                })
                .collect(),
        );
        held.insert(
            partition,
            LanePointer {
                open: open_seq,
                range: merged,
                disabled,
            },
        );
    }
    for (family, held) in lanes {
        batch = batch.put(
            PointerKey::keyspace(),
            PointerKey::new(namespace, database, table, family, version).encode(),
            Value::from(lanes_to_bytes(&held)),
        );
    }
    for (id, entries) in maps {
        batch = batch.put(
            MapKey::keyspace(),
            MapKey::new(namespace, database, table, id, version).encode(),
            Value::from(map_bytes(&entries)),
        );
    }
    Ok(batch)
}

/// The record's stored payload decoded to an object, or `None` for an absent
/// or non-object record — only objects have fields to project.
async fn held_object(
    view: &crate::transaction::Transaction<'_>,
    address: &RecordAddress,
) -> Result<Option<BTreeMap<String, RecordValueType>>> {
    Ok(view
        .get_held(address)
        .await?
        .and_then(|payload| decode_object(&payload).ok().flatten()))
}

/// Decode stored payload bytes to an object, `None` when they hold something
/// else — a record in a columnar table that is not an object has no fields,
/// and the row answers for it whole.
fn decode_object(payload: &[u8]) -> Result<Option<BTreeMap<String, RecordValueType>>> {
    match decode_payload(payload).map_err(crate::Error::from)? {
        RecordValueType::Object(object) => Ok(Some(object)),
        _ => Ok(None),
    }
}

/// Load one lane's writer from the committed state: the family's pointer
/// names every lane's open chunk seq, and the named lane's newest chunk
/// version becomes the rows appends continue . The lanes map is the
/// caller's, seeded from the pointer when its lane is first touched, so one
/// pointer read serves the commit.
#[allow(clippy::too_many_arguments)]
async fn load_writer(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    family: &ColumnFamily,
    partition: u64,
    lanes: &mut BTreeMap<u64, LanePointer>,
) -> Result<ChunkWriter> {
    if lanes.is_empty() {
        *lanes = match newest_under(
            store,
            &PointerKey::versions_prefix(namespace, database, table, family),
        )
        .await?
        {
            Some((_, bytes)) => lanes_from_bytes(&bytes)?,
            None => BTreeMap::new(),
        };
    }
    let local = lanes
        .get(&partition)
        .map(|lane| local_of_seq(lane.open))
        .unwrap_or(0);
    match newest_under(
        store,
        &ChunkKey::versions_prefix(
            namespace,
            database,
            table,
            family,
            split_seq(partition, local),
        ),
    )
    .await?
    {
        Some((_, bytes)) => {
            let chunk = super::chunk::decode_chunk(&bytes)?;
            Ok(ChunkWriter::loaded(local, chunk.record_ids, chunk.values))
        }
        None => Ok(ChunkWriter::loaded(local, Vec::new(), Vec::new())),
    }
}

/// The copy-on-write an update owes a sealed chunk holding the record's row:
/// that chunk's newest version with the row removed (§5.2).
#[allow(clippy::too_many_arguments)]
async fn cow_row(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    family: &ColumnFamily,
    id: &RecordId,
) -> Result<Option<(u64, Vec<u8>)>> {
    let Some(entry) = read_map(store, namespace, database, table, id).await? else {
        return Ok(None);
    };
    let Some(seq) = entry
        .into_iter()
        .find_map(|(held, seq)| (held == *family).then_some(seq))
    else {
        return Ok(None);
    };
    let Some((_, bytes)) = newest_under(
        store,
        &ChunkKey::versions_prefix(namespace, database, table, family, seq),
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(remove_row(&bytes, id)?.map(|rewritten| (seq, rewritten)))
}

/// The record's map entry as (family, chunk seq) pairs, `None` when the
/// record holds none.
pub(crate) async fn read_map(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    id: &RecordId,
) -> Result<Option<Vec<(ColumnFamily, u64)>>> {
    let Some((_, bytes)) = newest_under(
        store,
        &MapKey::versions_prefix(namespace, database, table, id),
    )
    .await?
    else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    let mut cursor = super::codec::Cursor::new(&bytes);
    let count = u32::from_be_bytes(cursor.take(4)?.try_into().map_err(|_| {
        crate::error::Error::ColumnarTruncated {
            needed: 4,
            len: bytes.len(),
        }
    })?);
    for _ in 0..count {
        let len = u32::from_be_bytes(cursor.take(4)?.try_into().map_err(|_| {
            crate::error::Error::ColumnarTruncated {
                needed: 4,
                len: bytes.len(),
            }
        })?);
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let family = ColumnFamily::from_value_bytes(cursor.take(len)?)?;
        let seq = u64::from_be_bytes(cursor.take(8)?.try_into().map_err(|_| {
            crate::error::Error::ColumnarTruncated {
                needed: 8,
                len: bytes.len(),
            }
        })?);
        entries.push((family, seq));
    }
    Ok(Some(entries))
}

/// Encode a map entry: the families a record's rows live in and the chunk
/// seq each lives at.
fn map_bytes(entries: &[(ColumnFamily, u64)]) -> Vec<u8> {
    let mut out = u32::try_from(entries.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    for (family, seq) in entries {
        let family_bytes = family.clone().into_value_bytes();
        out.extend_from_slice(
            &u32::try_from(family_bytes.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        out.extend_from_slice(&family_bytes);
        out.extend_from_slice(&seq.to_be_bytes());
    }
    out
}

/// The newest version under a versions prefix — the first entry the
/// descending-version order hands a forward scan — as (version, bytes).
pub(crate) async fn newest_under(
    store: &Store,
    prefix: &[u8],
) -> Result<Option<(Sequence, Vec<u8>)>> {
    let request = ScanRequest {
        keyspace: ChunkKey::keyspace(),
        range: KeyRange::prefix(prefix),
        direction: ScanDirection::Forward,
        limit: Some(1),
    };
    let found = store.backend().scan(&request).await?;
    let Some((key, value)) = found.first() else {
        return Ok(None);
    };
    let bytes = key.as_slice();
    let start = bytes.len().saturating_sub(8);
    let raw: [u8; 8] = bytes
        .get(start..)
        .and_then(|tail| tail.try_into().ok())
        .ok_or(crate::error::Error::ColumnarTruncated {
            needed: 8,
            len: bytes.len(),
        })?;
    // The suffix is the complement of the sequence.
    Ok(Some((
        Sequence::new(!u64::from_be_bytes(raw)),
        value.as_slice().to_vec(),
    )))
}
