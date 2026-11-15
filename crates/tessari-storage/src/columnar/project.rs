//! The columnar projection's commit-batch derivation (worksheet §5).
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
//!   schemaless record projects the keys it carries (D-SG2-2, refined by
//!   D-SG2-10: a record covers the families it has values for).
//! - **Update** — a field whose value changed is a row replaced in the open
//!   chunk, or, when the row sits in a sealed chunk, a copy-on-write rewrite
//!   of that chunk (the value removed) and an append to the open one (§5.2).
//!   A field whose value is unchanged writes nothing.
//! - **Delete** — nothing. The row tombstone is the authority for visibility
//!   and a scan re-evaluates it (§5.2, §5.4); rewriting the chunk would be
//!   the vacuum §5.4 defers (D-SG2-9). The `'M'` entry stays: it points at
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

/// The partition a record's values route to, `0` when the table names no
/// partitioning. The hash is the murmur2 both consumers share, over the
/// payload encoding of the partition field's value — the normal form
/// comparison reduces to, so equal values route equal. A null or absent
/// value routes to partition 0 (D-SG4-3: analytics narrows, it does not
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

/// A family's pointer value: the open chunk seq of every partition lane it
/// holds, `[count:u32][(partition:u32, open:u64) * count]` in partition
/// order. An eight-byte value is the pre-partitioning form — one lane at
/// partition 0 — so a dev store written before SG4 still reads.
pub(crate) fn lanes_to_bytes(lanes: &BTreeMap<u64, u64>) -> Vec<u8> {
    let mut out = u32::try_from(lanes.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    for (partition, open) in lanes {
        out.extend_from_slice(&u32::try_from(*partition).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&open.to_be_bytes());
    }
    out
}

/// The lanes a pointer value names — [`lanes_to_bytes`] read back, with the
/// eight-byte legacy form tolerated.
pub(crate) fn lanes_from_bytes(bytes: &[u8]) -> Result<BTreeMap<u64, u64>> {
    let mut lanes = BTreeMap::new();
    if bytes.len() == 8 {
        lanes.insert(
            0,
            u64::from_be_bytes(bytes.try_into().map_err(|_| {
                crate::error::Error::ColumnarTruncated {
                    needed: 8,
                    len: bytes.len(),
                }
            })?),
        );
        return Ok(lanes);
    }
    let mut cursor = super::codec::Cursor::new(bytes);
    let count = u32::from_be_bytes(cursor.take(4)?.try_into().map_err(|_| {
        crate::error::Error::ColumnarTruncated {
            needed: 4,
            len: bytes.len(),
        }
    })?);
    for _ in 0..count {
        let partition = u32::from_be_bytes(cursor.take(4)?.try_into().map_err(|_| {
            crate::error::Error::ColumnarTruncated {
                needed: 4,
                len: bytes.len(),
            }
        })?);
        let open = u64::from_be_bytes(cursor.take(8)?.try_into().map_err(|_| {
            crate::error::Error::ColumnarTruncated {
                needed: 8,
                len: bytes.len(),
            }
        })?);
        lanes.insert(u64::from(partition), open);
    }
    cursor.finish()?;
    Ok(lanes)
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
    let mut lanes: BTreeMap<ColumnFamily, BTreeMap<u64, u64>> = BTreeMap::new();
    let mut maps: Vec<(RecordId, Vec<(ColumnFamily, u64)>)> = Vec::new();
    let mut cows: Vec<(u64, ColumnFamily, Vec<u8>)> = Vec::new();

    for mutation in mutations {
        let address = RecordAddress::new(namespace, database, table, mutation.id.clone());
        let old = held_object(view, &address).await?;
        let new = match mutation.value.value() {
            RecordValue::Present(payload) => decode_object(payload)?,
            RecordValue::Tombstone => None,
        };
        // The row's values route to one partition's lane; the values it is
        // losing route from the lane they sit in (the object it held).
        let partition = partition_of(definition, new.as_ref().or(old.as_ref()))?;
        // The families this record touches: every declared family for a
        // schemafull table; every key the old or new object carries for a
        // schemaless one (D-SG2-10 — a record covers the families it has
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
            if old_value == new_value {
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
    // the pointer moved to each family's open seq, and the map entries — all
    // versioned puts in the one batch.
    let mut batch = batch;
    for (seq, family, bytes) in cows {
        batch = batch.put(
            ChunkKey::keyspace(),
            ChunkKey::new(namespace, database, table, family, seq, version).encode(),
            Value::from(bytes),
        );
    }
    for ((family, partition), writer) in writers {
        let open_seq = split_seq(partition, writer.seq());
        for sealed in writer.finish() {
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
        lanes
            .entry(family.clone())
            .or_default()
            .insert(partition, open_seq);
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
/// version becomes the rows appends continue (D-SG2-1). The lanes map is the
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
    lanes: &mut BTreeMap<u64, u64>,
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
        .map(|open| local_of_seq(*open))
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
