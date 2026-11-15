//! Reading the projection back: one family's chunks in append order, and one
//! record's projected values (SG2 T2.3).
//!
//! This is the decode half the columnar scan will ride in SG3; here it proves
//! the chunk is a faithful projection of the row — the exactness contract's
//! unit level (worksheet §6.4: the differential mirror compares what the
//! chunks hold against what the row path answers, and the two must be
//! identical).
//!
//! Reads are at the committed state: the pointer names a family's open chunk,
//! and chunks `0..=open` are read newest-version-first per seq. Snapshot
//! bounding is SG3's scan concern; these helpers answer "what the projection
//! holds now", which is what the equivalence suite needs.

use std::collections::BTreeMap;

use tessari_encoding::{ChunkKey, ColumnFamily, PointerKey};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

use crate::error::Result;
use crate::store::Store;

use super::chunk::{ColumnChunk, decode_chunk};
use super::project::{newest_under, read_map};

/// One family's chunks in append order, with each chunk's seq.
pub(crate) async fn family_chunks(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    family: &ColumnFamily,
) -> Result<Vec<(u64, ColumnChunk)>> {
    let open = match newest_under(
        store,
        &PointerKey::versions_prefix(namespace, database, table, family),
    )
    .await?
    {
        Some((_, bytes)) => super::project::seq_of(&bytes),
        None => return Ok(Vec::new()),
    };
    let mut chunks = Vec::new();
    let mut seq = 0;
    while seq <= open {
        if let Some((_, bytes)) = newest_under(
            store,
            &ChunkKey::versions_prefix(namespace, database, table, family, seq),
        )
        .await?
        {
            chunks.push((seq, decode_chunk(&bytes)?));
        }
        seq = seq.saturating_add(1);
    }
    Ok(chunks)
}

/// One record's projected values: the families it holds rows in and the value
/// each holds. A family whose chunk mentions the record is read through; a
/// record absent from the projection answers nothing here — the row path is
/// the authority and §6.1's non-qualifying reads answer through it.
pub(crate) async fn record_values(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    id: &RecordId,
) -> Result<BTreeMap<ColumnFamily, Value>> {
    let mut values = BTreeMap::new();
    let Some(entries) = read_map(store, namespace, database, table, id).await? else {
        return Ok(values);
    };
    for (family, seq) in entries {
        let Some((_, bytes)) = newest_under(
            store,
            &ChunkKey::versions_prefix(namespace, database, table, &family, seq),
        )
        .await?
        else {
            continue;
        };
        let chunk = decode_chunk(&bytes)?;
        if let Some(at) = chunk.record_ids.iter().position(|held| held == id)
            && let Some(value) = chunk.values.get(at)
        {
            values.insert(family, value.clone());
        }
    }
    Ok(values)
}
