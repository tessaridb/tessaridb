//! Building an index over what a table already holds, and clearing one.

use super::{
    Delta, Moved, Pending, analysed, analyzer_for, analyzers_named, analyzers_on, covering_of,
    insert, lengthen, place_cells, project, projected_vector,
};
use crate::catalog::IndexDefinition;
use crate::covering;
use crate::error::Result;
use crate::graph;
use crate::store::Store;
use crate::transaction::Transaction;
use std::collections::{BTreeMap, BTreeSet};
use tessari_encoding::{
    IndexAddress, IndexValues, KeyKind, LogRecord, PostingKey, RecordValue, SearchSuffixKey,
    SearchSurfaceKey, StoreKey, decode_payload,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::RecordId;

/// Every entry an index implies, written into the commit that defines — or
/// rebuilds — it.
///
/// The rows are the committed ones as of `view`, overlaid with this record's own
/// mutations for that table — so a script that defines an index and writes to the
/// table in one transaction indexes both what was there and what it just wrote.
///
/// # A build is authoritative, not additive
///
/// It makes the index's entries **be** what the rows imply, rather than adding
/// what the rows imply to whatever is already there. So it clears first, and a
/// search index's statistics come out as a total rather than a movement.
///
/// That is what a *rebuild* is, and it is why rebuilding needs no second path
/// and no new log shape: `REBUILD INDEX` writes the index's catalog record
/// again, unchanged, and a catalog record is an ordinary record (ADR-0009), so
/// this arrives exactly as a definition does. One rule covers both, and there
/// is no first-time-only branch left to be wrong.
///
/// A first build has nothing to clear and no statistics to reset, so the extra
/// work it pays for is four scans over an empty range — next to reading the
/// whole table, which it already does.
pub(crate) fn build(
    store: &Store,
    mut batch: WriteBatch,
    view: &mut Transaction<'_>,
    record: &LogRecord,
    definition: &IndexDefinition,
    pending: &mut Pending,
) -> Result<WriteBatch> {
    let address = IndexAddress::new(
        definition.namespace,
        definition.database,
        definition.table,
        definition.id,
    );
    batch = clear(store, batch, &address)?;
    pending.built.insert(address);
    // Anything the per-mutation pass moved for this index is discarded with the
    // entries it described: the rows below include this record's own mutations,
    // so counting them again is counting them twice.
    pending.moved.insert(address, Delta::default());
    pending.terms.insert(address, BTreeMap::new());
    pending.lengths.remove(&address);
    let mut rows: BTreeMap<RecordId, Vec<u8>> = view
        .sweep_table(definition.namespace, definition.database, definition.table)?
        .into_iter()
        .collect();

    for mutation in record.mutations() {
        if mutation.namespace != definition.namespace
            || mutation.database != definition.database
            || mutation.table != definition.table
        {
            continue;
        }
        match mutation.value.value() {
            RecordValue::Present(payload) => rows.insert(mutation.id.clone(), payload.clone()),
            RecordValue::Tombstone => rows.remove(&mutation.id),
        };
    }

    if let Some(distance) = definition.vector {
        // The graph is built in the commit that defines the index, the same way
        // every other index is — so a definition over a populated table and a
        // definition over an empty one followed by writes reach the same state.
        let mut graph = graph::Graph::empty(distance, definition.quantized);
        let mut written: BTreeMap<RecordId, tessari_encoding::VectorNode> = BTreeMap::new();
        for (id, payload) in &rows {
            let Some(held) = projected_vector(definition, &decode_payload(payload)?) else {
                continue;
            };
            written.extend(graph.insert(id, held)?);
        }
        // Measured here and nowhere else: this is the one place the whole graph
        // and every stored vector are in hand at once, and it is reached by
        // applying a log record, so every replica computes the same figure.
        let batch = graph::write(batch, &address, &written);
        return graph::measure(batch, &address, &graph);
    }

    if definition.spatial {
        // Measured here and nowhere else, for the reason the vector branch above
        // states: this is the one place every geometry and its covering are in
        // hand at once, and it is reached by applying a log record, so every
        // replica computes the same figure. A figure accumulated from real reads
        // would differ per replica by construction.
        let mut placed = Vec::with_capacity(rows.len());
        for (id, payload) in &rows {
            let held = decode_payload(payload)?;
            if let Some((bounds, cells)) = covering_of(definition, &held) {
                batch = place_cells(batch, &address, id, bounds, &cells);
                placed.push(covering::Placed {
                    id: id.clone(),
                    bounds,
                    cells,
                });
            }
        }
        return Ok(covering::measure(batch, &address, &placed));
    }

    if definition.search || definition.engine.is_some() {
        let declared = analyzers_on(view, definition.table)?;
        let named = if definition.engine.is_some() {
            analyzers_named(view)?
        } else {
            BTreeMap::new()
        };
        let analyzer = analyzer_for(definition, &declared, &named);
        let mut counted = Delta::default();
        let mut dictionary: BTreeMap<IndexValues, Moved> = BTreeMap::new();
        let mut surfaces = BTreeMap::new();
        for (id, payload) in &rows {
            let mut analysed = analysed(definition, analyzer, &decode_payload(payload)?);
            counted.added(analysed.tokens);
            lengthen(&mut pending.lengths, address, &analysed.fields, true);
            for pair in std::mem::take(&mut analysed.surfaces) {
                let held: &mut i64 = surfaces.entry(pair).or_default();
                *held = held.saturating_add(1);
            }
            let length = analysed.length();
            for ((term, frequency), located) in analysed.postings.into_iter().zip(&analysed.located)
            {
                dictionary
                    .entry(term.clone())
                    .or_default()
                    .arrived(frequency, length);
                batch = batch.put(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, id.clone()).encode(),
                    super::posted(definition, frequency, length, located),
                );
            }
        }
        if !definition.costs.unscored {
            *pending.moved.entry(address).or_default() = counted;
        }
        pending.terms.insert(address, dictionary);
        pending.surfaces.insert(address, surfaces);
        // The suffixes of every term are written as the dictionary settles; this
        // marker says they are complete for this index, which an index built
        // before suffixes existed cannot say (ADR-0105 D9). The surfaces carry
        // the same kind of marker for the same reason (Q-867).
        return Ok(batch
            .put(
                SearchSuffixKey::keyspace(),
                SearchSuffixKey::new(address, String::new(), String::new()).encode(),
                SearchSuffixKey::empty(),
            )
            .put(
                SearchSurfaceKey::keyspace(),
                SearchSurfaceKey::marker(address),
                SearchSurfaceKey::count(0),
            ));
    }

    // A claim set of this build's own. The per-mutation pass may have claimed
    // values in this same index — and those claims describe entries the clear
    // above has just removed, so honouring them would refuse a rebuild for
    // colliding with itself. Nothing is lost: every row is seen here, so a
    // genuine duplicate still collides.
    let mut claimed = BTreeSet::new();
    for (id, payload) in &rows {
        for values in project(definition, &decode_payload(payload)?) {
            batch = insert(
                store,
                batch,
                definition,
                &address,
                &values,
                id,
                &mut claimed,
            )?;
        }
    }
    Ok(batch)
}

/// Every entry this index currently holds, deleted from the batch.
///
/// A build writes what the rows imply; without this it would leave behind
/// whatever an earlier build left — a node whose neighbour list points at
/// records that have gone, a posting for text nobody stores any more, an entry
/// under a value the record no longer holds.
///
/// All eight index key kinds, because a rebuild has to be safe on any index a
/// caller may name, and an index whose shape changed is not a case this store
/// wants to reason about one kind at a time.
///
/// The measurements are cleared with the entries for a reason worth stating: a
/// recall or a refinement figure left behind would describe a graph or a
/// covering that no longer exists, which is exactly the stale number the
/// measurements were introduced to prevent, arriving from inside. Neither fails
/// a test until somebody reads it.
pub(crate) fn clear(
    store: &Store,
    mut batch: WriteBatch,
    address: &IndexAddress,
) -> Result<WriteBatch> {
    for kind in [
        KeyKind::SecondaryIndex,
        KeyKind::UniqueIndex,
        KeyKind::Posting,
        KeyKind::VectorNode,
        KeyKind::SearchStatistics,
        KeyKind::SpatialIndex,
        KeyKind::VectorRecall,
        KeyKind::SpatialRefinement,
        KeyKind::SearchTerm,
        KeyKind::SearchSuffix,
        KeyKind::SearchSurface,
        KeyKind::IndexStatistics,
        KeyKind::IndexChanges,
    ] {
        let keyspace = kind.keyspace();
        let prefix = address.prefix(kind);
        let request = ScanRequest {
            keyspace,
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        for (key, _) in store.backend().scan(&request)? {
            batch = batch.delete(keyspace, key);
        }
    }
    Ok(batch)
}
